"""Rewrite one file header of a RAR5 archive (used by make-rar-fixtures.sh).

usage: python rar5patch.py IN OUT OLD_NAME [--name NEW_NAME] [--dict-log N]

Finds the file header named OLD_NAME, optionally gives it NEW_NAME and/or sets its
dictionary size field to 128 KiB << N, then re-encodes the header (size and CRC32).
Only the header changes; the file's data and its own checksum are left as they are.
"""
import struct
import sys
import zlib

SIG = b"Rar!\x1a\x07\x01\x00"


def vint(data, pos):
    value, shift = 0, 0
    while True:
        b = data[pos]
        pos += 1
        value |= (b & 0x7F) << shift
        shift += 7
        if not b & 0x80:
            return value, pos


def enc(value):
    out = bytearray()
    while True:
        b = value & 0x7F
        value >>= 7
        if value:
            out.append(b | 0x80)
        else:
            out.append(b)
            return bytes(out)


def main():
    src, dst, old = sys.argv[1], sys.argv[2], sys.argv[3].encode("utf-8")
    new_name, dict_log = None, None
    args = sys.argv[4:]
    while args:
        if args[0] == "--name":
            new_name = args[1].encode("utf-8")
        elif args[0] == "--dict-log":
            dict_log = int(args[1])
        else:
            raise SystemExit(f"unknown option {args[0]}")
        args = args[2:]
    data = bytearray(open(src, "rb").read())
    assert data.startswith(SIG), "not a RAR5 archive"
    pos = len(SIG)
    while pos < len(data):
        start = pos
        pos += 4  # CRC32
        size, body = vint(data, pos)
        end = body + size
        p = body
        htype, p = vint(data, p)
        hflags, p = vint(data, p)
        extra_size = data_size = 0
        if hflags & 1:
            extra_size, p = vint(data, p)
        if hflags & 2:
            data_size, p = vint(data, p)
        if htype == 2:
            fields_start = p
            fflags, p = vint(data, p)
            _unp, p = vint(data, p)
            _attr, p = vint(data, p)
            if fflags & 2:
                p += 4
            if fflags & 4:
                p += 4
            comp_at = p
            comp, p = vint(data, p)
            _host, p = vint(data, p)
            name_len, p = vint(data, p)
            name = bytes(data[p:p + name_len])
            name_end = p + name_len
            if name == old:
                if dict_log is not None:
                    comp = (comp & ~(0x1F << 10)) | (dict_log << 10)
                if new_name is not None:
                    name = new_name
                head = bytes(data[body:fields_start])  # type, flags, extra/data sizes
                fields = (bytes(data[fields_start:comp_at]) + enc(comp)
                          + bytes(data[comp_at + len(enc(vint(data, comp_at)[0])):name_end - name_len - len(enc(name_len))])
                          + enc(len(name)) + name)
                rest = bytes(data[name_end:end])  # extra area
                inner = head + fields + rest
                header = enc(len(inner)) + inner
                crc = struct.pack("<I", zlib.crc32(header) & 0xFFFFFFFF)
                data[start:end] = crc + header
                open(dst, "wb").write(bytes(data))
                return
        pos = end + data_size
    raise SystemExit(f"no file header named {old!r}")


main()
