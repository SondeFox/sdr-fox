# SPDX-License-Identifier: MIT OR Apache-2.0
"""Candidate-only checks of real ELF headers and final rustc link arguments."""
import struct


def require(condition, message):
    if not condition:
        raise ValueError(message)


def check_link_args(argv, required):
    codegen = []
    index = 0
    while index < len(argv):
        argument = argv[index]
        if argument in ("-C", "--codegen"):
            index += 1
            require(index < len(argv), "Truncated rustc codegen argument")
            codegen.append(argv[index])
        elif argument.startswith("-C"):
            codegen.append(argument[2:])
        elif argument.startswith("--codegen="):
            codegen.append(argument[len("--codegen="):])
        index += 1
    observed = [value for value in codegen if "page-size" in value]
    wanted = ["link-arg=" + value for value in required]
    require(sorted(observed) == sorted(wanted), "Missing, duplicate or conflicting Android page-size link argument")
    return list(required)


def inspect_layout(data):
    require(len(data) >= 64 and data[:7] == b"\x7fELF\x02\x01\x01", "Expected little-endian ELF64")
    phoff = struct.unpack_from("<Q", data, 32)[0]
    ehsize, phsize, phnum = struct.unpack_from("<HHH", data, 52)
    require(ehsize == 64 and phsize == 56 and 0 < phnum < 65535 and
            phoff >= 64 and phoff + phsize * phnum <= len(data), "Invalid ELF program-header table")
    loads, relros = [], []
    for i in range(phnum):
        kind, flags, offset, address, _, filesz, memsz, alignment = struct.unpack_from("<IIQQQQQQ", data, phoff + i * phsize)
        require(offset + filesz <= len(data) and address + memsz < 2 ** 64, "ELF segment exceeds file or address bounds")
        if kind == 1:
            require(0 < memsz and filesz <= memsz, "Invalid LOAD size")
            require(alignment >= 16384 and alignment & (alignment - 1) == 0 and
                    address % alignment == offset % alignment, "LOAD lacks 16 KB alignment")
            loads.append((flags, address, address + memsz))
        elif kind == 0x6474e552:
            # RELRO is a protection range; FileSiz may exceed MemSiz.
            require(memsz > 0 and (address + memsz) % 16384 == 0, "RELRO end lacks 16 KB alignment")
            relros.append((address, address + memsz))
    require(loads and len(relros) <= 1, "Missing LOAD or duplicate RELRO")
    for start, end in relros:
        require(any(flags & 2 and low <= start <= end <= high for flags, low, high in loads), "RELRO is outside a writable LOAD")
    return {"load_segments": len(loads), "relro_segments": len(relros), "page_size_bytes": 16384}
