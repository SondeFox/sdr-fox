// SdrUsbManager.kt — Android USB-Host permission + fd helper.
//
// Mirrors SondeFox's AirspyUsbManager: request USB permission for an SDR
// device, then open it and hand the file descriptor to SdrFox.open().
//
// Permission flags follow Android's evolution:
//   - FLAG_MUTABLE on API 31+ (Android 12+) for the PendingIntent.
//   - RECEIVER_NOT_EXPORTED on API 33+ (Android 13+) for the BroadcastReceiver.

package com.sdrfox

import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.hardware.usb.UsbDevice
import android.hardware.usb.UsbDeviceConnection
import android.hardware.usb.UsbManager
import android.os.Build

/** Known SDR USB VID:PID pairs. */
object SdrUsbIds {
    val RTL_SDR = listOf(
        0x0bda to 0x2832,
        0x0bda to 0x2838,
        0x1d50 to 0x6089,
        0x1d50 to 0xcc60,
    )
    val AIRSPY = listOf(0x1d50 to 0x60a1)
    val ALL = RTL_SDR + AIRSPY

    fun matches(device: UsbDevice): SdrFox.Kind? {
        val vid = device.vendorId
        val pid = device.productId
        // Enumeration can happen before USB permission; some Android builds
        // throw rather than returning null for productName in that state.
        val productName = try {
            device.productName
        } catch (_: SecurityException) {
            null
        }
        return when {
            RTL_SDR.any { it.first == vid && it.second == pid } -> SdrFox.Kind.RTL_SDR
            AIRSPY.any { it.first == vid && it.second == pid } &&
                productName?.contains("mini", ignoreCase = true) == true ->
                SdrFox.Kind.AIRSPY_MINI
            AIRSPY.any { it.first == vid && it.second == pid } -> SdrFox.Kind.AIRSPY
            else -> null
        }
    }
}

/**
 * Request USB permission for [device]. Calls [onResult] with true if granted.
 *
 * Usage:
 *   SdrUsbPermission.request(context, usbManager, device) { granted ->
 *       if (granted) { val conn = usbManager.openDevice(device); ... }
 *   }
 */
object SdrUsbPermission {

    private const val ACTION_USB_PERMISSION = "com.sdrfox.USB_PERMISSION"

    fun request(
        context: Context,
        usbManager: UsbManager,
        device: UsbDevice,
        onResult: (Boolean) -> Unit,
    ) {
        val flag = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_MUTABLE
        } else {
            PendingIntent.FLAG_UPDATE_CURRENT
        }
        val intent = PendingIntent.getBroadcast(
            context,
            0,
            Intent(ACTION_USB_PERMISSION).setPackage(context.packageName),
            flag,
        )

        val receiver = object : BroadcastReceiver() {
            override fun onReceive(ctx: Context, intent: Intent) {
                if (ACTION_USB_PERMISSION == intent.action) {
                    val granted = intent.getBooleanExtra(UsbManager.EXTRA_PERMISSION_GRANTED, false)
                    try { ctx.unregisterReceiver(this) } catch (_: Exception) {}
                    onResult(granted)
                }
            }
        }

        val filter = IntentFilter(ACTION_USB_PERMISSION)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            context.registerReceiver(receiver, filter, Context.RECEIVER_NOT_EXPORTED)
        } else {
            @Suppress("UnspecifiedRegisterReceiverFlag")
            context.registerReceiver(receiver, filter)
        }

        usbManager.requestPermission(device, intent)
    }
}

/**
 * Open a [device] after permission and return its connection plus the file
 * descriptor expected by [SdrFox.open]. The caller must keep the connection
 * open for the entire [SdrFox] lifetime and close it second.
 */
fun UsbManager.openSdr(device: UsbDevice): Pair<UsbDeviceConnection, Int>? {
    val connection = openDevice(device) ?: return null
    val fd = connection.fileDescriptor
    return if (fd >= 0) {
        connection to fd
    } else {
        connection.close()
        null
    }
}
