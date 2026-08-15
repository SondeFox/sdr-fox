"""sdr-fox Python package.

The native extension (``sdr_fox``) is built by maturin from the Rust crate in
this directory. After ``maturin develop`` you can::

    import sdr_fox
    out = sdr_fox.convert_cu8_to_cf32(bytes([0, 255]))

For device access::

    with sdr_fox.SdrFox.open(0, kind="rtl-sdr") as sdr:
        sdr.frequency = 100e6
        block = sdr.read(format="cf32")
"""
from .sdr_fox import *  # noqa: F401,F403  re-export native symbols
from .sdr_fox import SdrFox, IqBlockPy, convert_cu8_to_cf32  # noqa: F401

__all__ = ["SdrFox", "IqBlockPy", "convert_cu8_to_cf32"]
