#!/usr/bin/env python3
"""Build reproducible release archives from proposed-version working files."""

import gzip
import io
import os
import struct
import subprocess
import sys
import tarfile
import zipfile
from pathlib import Path


def validate_binary(path, platform, arch):
    """Reject incorrectly labelled ELF/PE executables before packaging."""
    with Path(path).open("rb") as stream:
        data = stream.read(4096)
        if platform == "linux":
            expected = {
                "amd64": 62,
                "x86_64": 62,
                "arm64": 183,
                "aarch64": 183,
                "armv7": 40,
                "386": 3,
                "i686": 3,
            }.get(arch)
            if data[:4] != b"\x7fELF" or len(data) < 20 or expected is None:
                raise ValueError(f"Expected a supported Linux ELF executable: {path}")
            machine = struct.unpack("<H" if data[5] == 1 else ">H", data[18:20])[0]
        elif platform == "windows":
            expected = {
                "amd64": 0x8664,
                "x86_64": 0x8664,
                "arm64": 0xAA64,
                "aarch64": 0xAA64,
                "386": 0x14C,
                "i686": 0x14C,
            }.get(arch)
            if data[:2] != b"MZ" or len(data) < 64 or expected is None:
                raise ValueError(f"Expected a supported Windows PE executable: {path}")
            offset = struct.unpack("<I", data[60:64])[0]
            stream.seek(offset)
            header = stream.read(6)
            if len(header) != 6 or header[:4] != b"PE\0\0":
                raise ValueError(f"Invalid Windows PE executable: {path}")
            machine = struct.unpack("<H", header[4:])[0]
        else:
            raise ValueError(f"No executable architecture validator for {platform}")
    if machine != expected:
        raise ValueError(f"Executable architecture does not match {platform}-{arch}: {path}")


def archive(kind, platform=None, arch=None, files=None):
    name = os.environ["RELEASE_NAME"]
    release_version = os.environ["RELEASE_VERSION"]
    out = Path(os.environ["RELEASE_DIR"])
    out.mkdir(parents=True, exist_ok=True)
    prefix = f"{name}.v{release_version}"
    entries = []
    if kind == "source":
        tracked = subprocess.check_output(["git", "ls-files", "-z"], text=True).split("\0")
        for filename in sorted(filter(None, tracked)):
            path = Path(filename)
            if path.parts[0] in {".forgejo", ".github", ".codex"}:
                continue
            entries.append((path, f"{prefix}/{path.as_posix()}"))
        # VERSION may be introduced during a first release; it is still required.
        if not any(p == Path("VERSION") for p, _ in entries):
            entries.append((Path("VERSION"), f"{prefix}/VERSION"))
        stem = prefix + "-source"
    elif kind == "binary" and platform in {"linux", "windows", "darwin"} and arch and files:
        validate_binary(files[0], platform, arch)
        stem = f"{prefix}-{platform}-{arch}"
        for filename in files:
            path = Path(filename)
            if path.is_dir():
                entries.extend(
                    (p, str(Path(path.name) / p.relative_to(path)).replace("\\", "/"))
                    for p in sorted(path.rglob("*"))
                    if not p.is_dir()
                )
            else:
                entries.append((path, path.name))
    else:
        raise ValueError(
            "Usage: package.py source | binary linux|windows|darwin ARCH FILE [FILE...]"
        )
    if len({n for _, n in entries}) != len(entries):
        raise ValueError("Archive paths must be unique")
    if not entries or any(not p.is_file() and not p.is_symlink() for p, _ in entries):
        raise ValueError("Archive input is missing")
    target = out / (stem + (".zip" if platform == "windows" else ".tar.gz"))
    if platform == "windows":
        with zipfile.ZipFile(target, "w", compression=zipfile.ZIP_DEFLATED) as z:
            for path, filename in entries:
                info = zipfile.ZipInfo(filename, (1980, 1, 1, 0, 0, 0))
                info.compress_type = zipfile.ZIP_DEFLATED
                info.external_attr = (0o755 if os.access(path, os.X_OK) else 0o644) << 16
                z.writestr(info, path.read_bytes())
    else:
        with (
            target.open("wb") as raw,
            gzip.GzipFile(fileobj=raw, mode="wb", filename="", mtime=0) as gz,
            tarfile.open(fileobj=gz, mode="w") as tar,
        ):
            for path, filename in entries:
                info = tarfile.TarInfo(filename)
                info.mode = 0o755 if os.access(path, os.X_OK) else 0o644
                if path.is_symlink():
                    info.type = tarfile.SYMTYPE
                    info.linkname = os.readlink(path)
                    tar.addfile(info)
                else:
                    data = path.read_bytes()
                    info.size = len(data)
                    tar.addfile(info, io.BytesIO(data))
    print(target)
    return target


if __name__ == "__main__":
    if sys.argv[1:] == ["source"]:
        archive("source")
    elif len(sys.argv) >= 5:
        archive(sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4:])
    else:
        raise SystemExit("Usage: package.py source | binary PLATFORM ARCH FILE [FILE...]")
