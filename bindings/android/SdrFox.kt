// SdrFox.kt — Kotlin wrapper around the sdr-fox native library.
//
// This is the consumer-facing Android API. The native `.so` is packaged into
// the APK via `externalNativeBuild` (see android/app/build.gradle.kts); the
// Kotlin class loads it and forwards to the JNI entry points.
//
// The Android USB-Host flow (permission + fd) lives in SdrUsbManager.kt; this
// class takes an already-obtained file descriptor from
// UsbDeviceConnection.getFileDescriptor().

package com.sdrfox

import android.os.ParcelFileDescriptor
import java.io.Closeable
import java.lang.ref.PhantomReference
import java.lang.ref.ReferenceQueue
import java.nio.ByteBuffer
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicLong
import java.util.concurrent.locks.ReentrantReadWriteLock

/**
 * An opened SDR device, backed by the native sdr-fox library.
 *
 * Lifecycle:
 *   val conn = usbManager.openDevice(device)  // obtain UsbDeviceConnection
 *   val fd = conn.fileDescriptor
 *   SdrFox.open(fd, SdrFox.Kind.RTL_SDR).use { sdr ->
 *       sdr.frequency = 100_000_000L
 *       sdr.biasTee = true
 *       ...
 *   }
 *   conn.close()   // AFTER the device — see [open]. Closing it earlier
 *                  // silently kills the bulk endpoint while control
 *                  // transfers keep working.
 *
 * The native handle and an owned duplicate of the Android USB fd are released
 * together. Cleanup uses a phantom-reference cleaner (compatible with the
 * library's API-29 minimum) rather than finalization. A read/write lock keeps
 * the fd alive through every native operation, while the native generational
 * registry independently makes close/use races memory-safe.
 */
class SdrFox private constructor(handle: Long, ownedFd: ParcelFileDescriptor) : Closeable {

    /** Hardware family selector for [open]. */
    enum class Kind(val native: Int) {
        /** RTL-SDR dongle. */
        RTL_SDR(0),
        /** Airspy R2 / Mini. */
        AIRSPY(1),
        /** Explicit Airspy Mini discriminator when USB product metadata is absent. */
        AIRSPY_MINI(2),
    }

    /** Native sample representation written into a stream's direct buffer. */
    enum class Format(val native: Int) {
        CU8(0),
        CS8(1),
        CS16(2),
        CF32(3),
    }

    /**
     * One analog gain stage in the tuner's signal chain.
     *
     * The [native] codes mirror sdr-fox-core's stable cross-language contract
     * (`GainStageId::code`): 0 = LNA, 1 = MIXER, 2 = VGA. Do not re-derive
     * this mapping elsewhere.
     */
    enum class GainStage(val native: Int) {
        /** Low-noise amplifier (RF front-end stage). */
        LNA(0),
        /** Mixer stage. */
        MIXER(1),
        /** Variable-gain amplifier (IF stage). */
        VGA(2),
    }

    /** Native transfer and queue sizing for [startStream]. */
    data class StreamConfig(
        val format: Format = Format.CU8,
        val bufferCount: Int = 16,
        val bufferSize: Int = 65_536,
        val queueDepth: Int = 32,
    ) {
        init {
            require(bufferCount > 0) { "bufferCount must be positive" }
            require(bufferSize > 0) { "bufferSize must be positive" }
            require(bufferSize % 512 == 0) { "bufferSize must be a multiple of 512" }
            require(queueDepth > 0) { "queueDepth must be positive" }
        }
    }

    /**
     * One coherent snapshot of stream loss, copy progress, and raw ADC clip
     * telemetry.
     *
     * Two kinds of counter live here — do not conflate them:
     * - **Monotonic** (accumulate since stream start): [lastDropped],
     *   [lastSequence], [blocksRead], [bytesRead], [totalClips],
     *   [totalRawSamples]. Diff successive snapshots for windowed figures,
     *   e.g. a clip fraction over a poll interval.
     * - **Per-block** (reset with every delivered block): [lastBlockClips],
     *   [lastBlockRawSamples] describe only the most recently received block.
     */
    data class StreamStats(
        /** Samples dropped due to consumer stalls since stream start (monotonic). */
        val lastDropped: Long,
        /** Sequence number of the most recent block (monotonic). */
        val lastSequence: Long,
        /** Blocks delivered through [SdrStream.read] since start (monotonic). */
        val blocksRead: Long,
        /** Bytes copied out through [SdrStream.read] since start (monotonic). */
        val bytesRead: Long,
        /**
         * Raw ADC-domain samples at/beyond the rails in the most recent block
         * only (**per-block**, resets every block; pre-filtering, so counted
         * before any decimation or conversion). 0 when the producer reports no
         * raw telemetry.
         */
        val lastBlockClips: Long,
        /**
         * Raw ADC-domain samples the most recent block was produced from
         * (**per-block**, resets every block; pre-decimation, so it can exceed
         * the delivered complex-sample count). The denominator for
         * [lastBlockClips]. 0 when the producer reports no raw telemetry.
         */
        val lastBlockRawSamples: Long,
        /** Sum of every block's clips since stream start (monotonic). */
        val totalClips: Long,
        /** Sum of every block's rawSamples since stream start (monotonic). */
        val totalRawSamples: Long,
    )

    private class NativeState(
        handle: Long,
        private val ownedFd: ParcelFileDescriptor,
    ) : Runnable {
        private val handle = AtomicLong(handle)
        private val lifecycle = ReentrantReadWriteLock()

        fun <T> withHandle(action: (Long) -> T): T {
            val lock = lifecycle.readLock()
            lock.lock()
            try {
                val current = handle.get()
                check(current != 0L) { "device closed" }
                return action(current)
            } finally {
                lock.unlock()
            }
        }

        override fun run() {
            val lock = lifecycle.writeLock()
            lock.lock()
            try {
                val current = handle.getAndSet(0L)
                if (current != 0L) {
                    nativeClose(current)
                }
                try {
                    ownedFd.close()
                } catch (_: Exception) {
                    // Cleanup is best-effort and idempotent.
                }
            } finally {
                lock.unlock()
            }
        }
    }

    private class StreamNativeState(handle: Long) : Runnable {
        private val handle = AtomicLong(handle)
        private val lifecycle = ReentrantReadWriteLock()

        fun read(
            buffer: ByteBuffer,
            offset: Int,
            length: Int,
            timeoutMs: Int,
        ): Int {
            val lock = lifecycle.readLock()
            lock.lock()
            try {
                val current = handle.get()
                check(current != 0L) { "stream closed" }
                return nativeReadStream(current, buffer, offset, length, timeoutMs)
            } finally {
                lock.unlock()
            }
        }

        fun fillStats(output: LongArray) {
            val lock = lifecycle.readLock()
            lock.lock()
            try {
                val current = handle.get()
                check(current != 0L) { "stream closed" }
                nativeStreamStats(current, output)
            } finally {
                lock.unlock()
            }
        }

        fun stop() {
            val current = handle.get()
            if (current != 0L) nativeStopStream(current)
        }

        override fun run() {
            // Signal before taking the write lock: a blocking read owns a read
            // lock and must be woken before cleanup can wait for it to exit.
            stop()
            val lock = lifecycle.writeLock()
            lock.lock()
            try {
                val current = handle.getAndSet(0L)
                if (current != 0L) nativeCloseStream(current)
            } finally {
                lock.unlock()
            }
        }
    }

    /**
     * A synchronous native IQ stream. [read] copies directly into the caller's
     * direct [ByteBuffer], advances its position, and creates no per-block Java
     * objects. Concurrent reads fail immediately rather than queueing behind an
     * unbounded native receive.
     */
    class SdrStream internal constructor(
        private val parent: SdrFox,
        handle: Long,
    ) : Closeable {
        private val state = StreamNativeState(handle)
        private val cleanable = NativeCleaner.register(this, state)
        private val statsScratch = LongArray(8)

        /**
         * Copy up to one IQ block into [buffer] and return the byte count.
         * `timeoutMs == 0` waits indefinitely; a finite timeout returns zero
         * without stopping the stream.
         */
        @JvmOverloads
        fun read(buffer: ByteBuffer, timeoutMs: Int = 0): Int {
            require(buffer.isDirect) { "buffer must be direct" }
            require(!buffer.isReadOnly) { "buffer must be writable" }
            require(timeoutMs >= 0) { "timeoutMs must be non-negative" }
            val position = buffer.position()
            val remaining = buffer.remaining()
            val copied = state.read(buffer, position, remaining, timeoutMs)
            check(copied in 0..remaining) { "native stream returned invalid byte count: $copied" }
            buffer.position(position + copied)
            return copied
        }

        /** Request cancellation while keeping close idempotent. */
        fun stop() {
            state.stop()
        }

        /**
         * Read all counters from one coherent native snapshot. See
         * [StreamStats] for which counters are monotonic and which are
         * per-block.
         */
        val stats: StreamStats
            get() = synchronized(statsScratch) {
                state.fillStats(statsScratch)
                StreamStats(
                    lastDropped = statsScratch[0],
                    lastSequence = statsScratch[1],
                    blocksRead = statsScratch[2],
                    bytesRead = statsScratch[3],
                    lastBlockClips = statsScratch[4],
                    lastBlockRawSamples = statsScratch[5],
                    totalClips = statsScratch[6],
                    totalRawSamples = statsScratch[7],
                )
            }

        override fun close() {
            cleanable.clean()
            parent.unregister(this)
        }
    }

    private val state = NativeState(handle, ownedFd)
    private val cleanable = NativeCleaner.register(this, state)
    private val streams = ConcurrentHashMap.newKeySet<SdrStream>()

    /** Center frequency in Hz (set only; the native side does not yet expose a getter). */
    var frequency: Long
        get() = throw UnsupportedOperationException("frequency is write-only")
        set(value) {
            state.withHandle { nativeSetFrequency(it, value) }
        }

    /**
     * Set the sample rate and return the rate the hardware actually settled
     * on, in Hz.
     *
     * The settled rate may differ from [hz] (integer-ratio synthesis on
     * RTL-SDR, a discrete firmware table on Airspy) and is the value every
     * downstream DSP consumer must size against — never assume the request
     * was applied verbatim.
     */
    fun setSampleRate(hz: Int): Int = state.withHandle { nativeSetSampleRate(it, hz) }

    /**
     * Set the analog channel bandwidth in Hz.
     *
     * On RTL-SDR this programs the R82xx IF filter, the matching RTL2832 IF
     * frequency and a retune as one atomic operation.
     *
     * **Call this after every [setSampleRate].** The tuner powers up with the
     * DVB-T filter (~6 MHz at IF 3.57 MHz) and nothing narrows it implicitly,
     * so a host that decimates a narrow channel out of a wide IF still exposes
     * the tuner's AGC detector to everything in that 6 MHz — which backs the
     * front end off and costs real sensitivity. Passing the *settled* rate
     * returned by [setSampleRate] reproduces librtlsdr's
     * `rtlsdr_set_tuner_bandwidth(dev, 0)` automatic behaviour.
     */
    fun setBandwidth(hz: Int) = state.withHandle { nativeSetBandwidth(it, hz) }

    /**
     * The discrete sample rates this device supports, in Hz, as a fresh array
     * per read.
     *
     * An **empty array means the rates are not enumerable** (e.g. RTL-SDR's
     * continuous synthesizer ranges): any in-range rate may be attempted via
     * [setSampleRate], which reports the settled rate. Airspy devices return
     * their firmware-queried table in firmware index order.
     */
    val supportedSampleRates: IntArray
        get() = state.withHandle { nativeGetSampleRates(it) }

    /** Enable/disable the bias tee (GPIO0 on RTL-SDR, RF bias on Airspy). */
    var biasTee: Boolean
        get() = throw UnsupportedOperationException("biasTee is write-only")
        set(value) {
            state.withHandle { nativeSetBiasTee(it, if (value) 1 else 0) }
        }

    /**
     * Enable/disable the **device-wide** AGC.
     *
     * On RTL-SDR this drives the RTL2832 **digital** AGC loop in the
     * demodulator — a different knob from the tuner's automatic gain mode,
     * which is [setGainMode]. On Airspy this switches the coupled LNA+mixer
     * AGC flags together; use [setStageAgc] to control one stage on its own.
     */
    var agc: Boolean
        get() = throw UnsupportedOperationException("agc is write-only")
        set(value) {
            state.withHandle { nativeSetAgc(it, if (value) 1 else 0) }
        }

    /**
     * Set the overall gain in tenths of dB (e.g. 400 = 40.0 dB).
     *
     * Devices that only accept per-stage gain (Airspy) reject overall
     * requests; use [setGainStage] for those.
     */
    fun setGain(tenthsDb: Int) {
        state.withHandle { nativeSetGain(it, tenthsDb) }
    }

    /**
     * Set the gain of a single analog [stage] in tenths of dB (e.g. 105 =
     * 10.5 dB). This is the only way to set gain on devices that reject
     * overall requests (Airspy).
     */
    fun setGainStage(stage: GainStage, tenthsDb: Int) {
        state.withHandle { nativeSetGainStage(it, stage.native, tenthsDb) }
    }

    /**
     * Switch the tuner between automatic ([auto] = true) and manual gain.
     *
     * This is the tuner's gain-mode knob (e.g. the R82xx auto gain used by
     * software AGC loops) — **not** the RTL2832 digital AGC, which stays on
     * [agc].
     */
    fun setGainMode(auto: Boolean) {
        state.withHandle { nativeSetGainMode(it, auto) }
    }

    /**
     * Enable/disable the AGC loop of a **single** gain stage, independently
     * of the others (e.g. Airspy's separate LNA and mixer AGC loops).
     *
     * Devices without per-stage AGC hardware (RTL-SDR), and stages without an
     * AGC loop (Airspy [GainStage.VGA]), throw. [agc] remains the coupled
     * device-wide switch.
     */
    fun setStageAgc(stage: GainStage, on: Boolean) {
        state.withHandle { nativeSetStageAgc(it, stage.native, on) }
    }

    /** Start a synchronous pull stream owned by this device. */
    @JvmOverloads
    fun startStream(config: StreamConfig = StreamConfig()): SdrStream = synchronized(streams) {
        val handle = state.withHandle {
            nativeStartStream(
                it,
                config.format.native,
                config.bufferCount,
                config.bufferSize,
                config.queueDepth,
            )
        }
        check(handle != 0L) { "native stream start failed" }
        SdrStream(this, handle).also { streams.add(it) }
    }

    private fun unregister(stream: SdrStream) {
        streams.remove(stream)
    }

    /**
     * Close the device and free the native handle. Idempotent: the first call
     * performs native and fd cleanup; subsequent calls are no-ops. Safe to call
     * concurrently with setters: cleanup waits for their read-side lifecycle
     * locks before closing the owned fd.
     */
    override fun close() {
        synchronized(streams) {
            // Native streams retain the parent device Arc for safety, but the
            // Kotlin owner still closes every wrapper deterministically.
            streams.toList().forEach { it.close() }
            cleanable.clean()
        }
    }

    companion object {
        init {
            System.loadLibrary("sdr_fox_jni")
        }

        /**
         * Open a device by file descriptor. The fd must come from
         * `UsbDeviceConnection.getFileDescriptor()` after the app has obtained
         * USB permission.
         *
         * **KEEP THE `UsbDeviceConnection` OPEN** until after [close]. This
         * method duplicates the fd, but `dup(2)` does not give the duplicate an
         * independent life: both descriptors share ONE open file description,
         * and usbfs hangs its per-connection state (claimed interfaces, URB
         * context) off that shared description. Closing the framework
         * connection early tears that state down underneath the native side.
         *
         * The failure mode is deceptive: the descriptor stays valid and control
         * transfers keep succeeding, so open, tuning, rate and gain all report
         * success — but the bulk endpoint goes permanently silent. Every
         * submitted URB sits pending until it is cancelled, and the device
         * looks wedged when nothing is wrong with it. This was observed
         * end-to-end on an RTL-SDR at 3.2 MS/s, where the same dongle streamed
         * normally as soon as the connection was held open.
         *
         * Close order at teardown: [close] first, then the `UsbDeviceConnection`.
         *
         * Pass [productName] from `UsbDevice.getProductName()` when available.
         * It is the reliable R2/Mini discriminator used by the Airspy backend;
         * alternatively select [Kind.AIRSPY_MINI] explicitly.
         * Board-specific receivers such as Blog V4 need the complete identity;
         * use [openUsbDevice] with the framework's actual USB metadata.
         */
        @JvmStatic
        @JvmOverloads
        fun open(
            fd: Int,
            kind: Kind = Kind.RTL_SDR,
            productName: String? = null,
        ): SdrFox? = openOwnedFd(fd) { owned ->
            nativeOpenByFd(owned, kind.native, productName)
        }

        /**
         * Open the same framework-authorized device represented by [fd]. Pass
         * its actual `UsbDevice` IDs and manufacturer/product strings after
         * permission is granted. Missing strings stay null; never substitute a
         * model name or infer Blog V4 from an R828D tuner or product string alone.
         * No serial number is needed or retained. The connection lifetime and
         * close ordering are identical to [open].
         */
        @JvmStatic
        fun openUsbDevice(
            fd: Int,
            kind: Kind,
            vendorId: Int,
            productId: Int,
            manufacturerName: String?,
            productName: String?,
        ): SdrFox? {
            require(vendorId in 0..0xffff) { "vendorId must be a USB uint16" }
            require(productId in 0..0xffff) { "productId must be a USB uint16" }
            return openOwnedFd(fd) { owned ->
                nativeOpenByFdWithIdentity(owned, kind.native, vendorId, productId,
                    manufacturerName, productName)
            }
        }

        private fun openOwnedFd(fd: Int, nativeOpen: (Int) -> Long): SdrFox? {
            if (fd < 0) return null
            val ownedFd = try {
                // fromFd duplicates rather than adopts, so the native nusb
                // transport has a descriptor whose lifetime is independent of
                // the framework connection that supplied `fd`.
                ParcelFileDescriptor.fromFd(fd)
            } catch (_: Exception) {
                return null
            }
            val h = try {
                nativeOpen(ownedFd.fd)
            } catch (error: Throwable) {
                try { ownedFd.close() } catch (_: Exception) {}
                throw error
            }
            if (h == 0L) {
                try { ownedFd.close() } catch (_: Exception) {}
                return null
            }
            return SdrFox(h, ownedFd)
        }

        @JvmStatic private external fun nativeOpenByFd(
            fd: Int,
            kind: Int,
            productName: String?,
        ): Long
        @JvmStatic private external fun nativeOpenByFdWithIdentity(
            fd: Int,
            kind: Int,
            vendorId: Int,
            productId: Int,
            manufacturerName: String?,
            productName: String?,
        ): Long
        @JvmStatic private external fun nativeSetFrequency(handle: Long, hz: Long)
        @JvmStatic private external fun nativeSetSampleRate(handle: Long, hz: Int): Int
        @JvmStatic private external fun nativeGetSampleRates(handle: Long): IntArray
        @JvmStatic private external fun nativeSetBandwidth(handle: Long, hz: Int)
        @JvmStatic private external fun nativeSetBiasTee(handle: Long, on: Int)
        @JvmStatic private external fun nativeSetAgc(handle: Long, on: Int)
        @JvmStatic private external fun nativeSetGain(handle: Long, tenthsDb: Int)
        @JvmStatic private external fun nativeSetGainStage(handle: Long, stage: Int, tenthsDb: Int)
        @JvmStatic private external fun nativeSetGainMode(handle: Long, auto: Boolean)
        @JvmStatic private external fun nativeSetStageAgc(handle: Long, stage: Int, on: Boolean)
        @JvmStatic private external fun nativeStartStream(
            deviceHandle: Long,
            format: Int,
            bufferCount: Int,
            bufferSize: Int,
            queueDepth: Int,
        ): Long
        @JvmStatic private external fun nativeReadStream(
            streamHandle: Long,
            buffer: ByteBuffer,
            offset: Int,
            length: Int,
            timeoutMs: Int,
        ): Int
        @JvmStatic private external fun nativeStopStream(streamHandle: Long)
        @JvmStatic private external fun nativeCloseStream(streamHandle: Long)
        @JvmStatic private external fun nativeStreamStats(streamHandle: Long, output: LongArray)
        @JvmStatic private external fun nativeClose(handle: Long)

        /**
         * API-29-compatible Cleaner equivalent. `java.lang.ref.Cleaner` is not
         * available on every supported Android release, so a daemon drains a
         * [ReferenceQueue] of phantom references. Each cleanup action is held
         * strongly without retaining its referent.
         */
        private object NativeCleaner {
            private val queue = ReferenceQueue<Any>()
            private val references = ConcurrentHashMap.newKeySet<Cleanable>()

            init {
                Thread({
                    while (true) {
                        try {
                            (queue.remove() as Cleanable).clean()
                        } catch (_: InterruptedException) {
                            // Cleanup is process-lifetime work; ignore interrupts.
                        }
                    }
                }, "sdr-fox-cleaner").apply {
                    isDaemon = true
                    start()
                }
            }

            fun register(owner: Any, action: Runnable): Cleanable {
                val cleanable = Cleanable(owner, action)
                references.add(cleanable)
                return cleanable
            }

            class Cleanable(
                owner: Any,
                private val action: Runnable,
            ) : PhantomReference<Any>(owner, queue) {
                private val cleaned = AtomicBoolean(false)

                fun clean() {
                    if (cleaned.compareAndSet(false, true)) {
                        action.run()
                    }
                    clear()
                    references.remove(this)
                }
            }
        }
    }
}
