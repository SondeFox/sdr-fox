# sdr-fox Python bindings

The package exposes the native `sdr_fox` extension built by Maturin.

```python
import sdr_fox

with sdr_fox.SdrFox.open(0, kind="rtl-sdr") as radio:
    radio.frequency = 100_000_000
    raw = radio.read(65_536, format="cf32")
```

`read(count, format)` keeps one stream alive across calls and returns at most
`count` bytes (exactly `count` during normal streaming). Any suffix of the USB
block is retained for the next call. Blocking USB work runs without the Python
GIL. Another Python thread may call `close()` to cancel a blocked read; a
second simultaneous `read()` fails immediately instead of waiting with the GIL.
Returned `bytes` objects implement Python's buffer protocol; interpret
`cf32` with `numpy.frombuffer(raw, dtype=np.float32)` and `cs16` with
`numpy.frombuffer(raw, dtype=np.int16)` on the native-endian platforms supported
by sdr-fox.

`convert_cu8_to_cf32(data)` likewise returns contiguous native-endian `f32`
bytes, avoiding one Python object per scalar.
