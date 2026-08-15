"""
sdr-fox Python bindings — demo and smoke tests.

Build with:  maturin develop --release  (from crates/sdr-fox-python)
Then:        pytest tests/test_bindings.py

The SIMD conversion helper is unit-testable without any hardware; the device
tests require an attached RTL-SDR and are skipped if no device is present.
"""
import struct

import pytest

import sdr_fox


def test_convert_cu8_to_cf32_full_scale():
    # Byte 0   → (0   - 127.5)/127.5 = -1.0
    # Byte 255 → (255 - 127.5)/127.5 ≈ +1.0
    raw = sdr_fox.convert_cu8_to_cf32(bytes([0, 255, 128, 127]))
    assert isinstance(raw, bytes)
    out = struct.unpack("=4f", raw)
    assert out[0] == pytest.approx(-1.0, abs=1e-6)
    assert out[1] == pytest.approx((255 - 127.5) / 127.5, abs=1e-6)
    assert out[2] == pytest.approx((128 - 127.5) / 127.5, abs=1e-6)
    assert out[3] == pytest.approx((127 - 127.5) / 127.5, abs=1e-6)


def test_convert_cu8_to_cf32_length_matches():
    inp = bytes(range(256))
    out = sdr_fox.convert_cu8_to_cf32(inp)
    assert len(out) == 256 * struct.calcsize("=f")
    assert len(memoryview(out).cast("f")) == 256


def test_module_has_sdrfox_class():
    assert hasattr(sdr_fox, "SdrFox")
    assert hasattr(sdr_fox, "IqBlockPy")
    assert hasattr(sdr_fox, "convert_cu8_to_cf32")


# --- hardware-gated tests (skip if no dongle) ---

def _open_first_rtlsdr():
    """Try to open index 0; return None if no device is present."""
    try:
        return sdr_fox.SdrFox.open(0, kind="rtl-sdr")
    except RuntimeError as e:
        message = str(e).lower()
        if (
            "not found" in message
            or "no device" in message
            or ("no matching" in message and "device at index" in message)
        ):
            return None
        raise


def test_hardware_open_and_set_frequency():
    sdr = _open_first_rtlsdr()
    if sdr is None:
        pytest.skip("no RTL-SDR attached")
    try:
        sdr.frequency = 100_000_000
    except Exception as e:
        # A macOS nusb control-transfer stall can surface here;
        # skip rather than fail so the binding API itself is what we test.
        if "stalled" in str(e).lower() or "transport" in str(e).lower():
            pytest.skip(f"hardware control-plane issue: {e}")
        raise
