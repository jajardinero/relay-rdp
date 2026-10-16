#!/usr/bin/env python3
"""Copy distro copyright/license notices for the libraries deployed by linuxdeploy."""

import re
import shutil
import subprocess
import sys
from pathlib import Path


def output(*args):
    return subprocess.check_output(args, text=True, stderr=subprocess.DEVNULL).strip()


def copy_notice(source, destination):
    destination.parent.mkdir(parents=True, exist_ok=True)
    if source.is_dir():
        shutil.copytree(source, destination, dirs_exist_ok=True)
    else:
        shutil.copy2(source, destination)


def main():
    appdir = Path(sys.argv[1])
    licenses = appdir / "usr/share/licenses/relay/bundled"
    licenses.mkdir(parents=True, exist_ok=True)
    if shutil.which("dpkg-query") and Path("/etc/debian_version").exists():
        manager = "debian"
        common = Path("/usr/share/common-licenses")
    elif shutil.which("pacman"):
        manager = "arch"
        common = Path("/usr/share/licenses/common")
    elif shutil.which("rpm"):
        manager = "rpm"
        common = None
    else:
        raise RuntimeError("License collection requires dpkg-query, pacman or rpm.")
    if common and common.is_dir():
        copy_notice(common, licenses / "system-common")

    # Resolve sonames to the original distro files, before RPATH/strip changes.
    libraries = {}
    for line in output("ldconfig", "-p").splitlines():
        if "=>" in line and "x86-64" in line:
            name, path = line.split("=>", 1)
            libraries.setdefault(name.split()[0], Path(path.strip()).resolve())
    records = ["Library\tPackage/version\tOriginal path or source"]
    packages = set()
    for library in sorted((appdir / "usr/lib").glob("*.so*")):
        if library.is_symlink():
            continue
        match = re.search(r"\(SONAME\).*?\[(.*?)\]", output("readelf", "-d", str(library)))
        soname = match[1] if match else library.name
        if soname == "libopenh264.so.8":
            records.append(f"{soname}\tOpenH264 2.6.0\thttps://github.com/cisco/openh264/tree/v2.6.0")
            continue
        original = libraries.get(soname)
        if original is None:
            raise RuntimeError(f"Cannot find the distro source of {soname} in ldconfig.")
        if manager == "debian":
            package = output("dpkg-query", "-S", str(original)).split(": ", 1)[0]
            version = output("dpkg-query", "-W", "-f=${Version}", package)
            notices = [Path("/usr/share/doc") / package.split(":")[0] / "copyright"]
        elif manager == "arch":
            package = output("pacman", "-Qoq", str(original))
            version = output("pacman", "-Q", package).split(maxsplit=1)[1]
            directory = Path("/usr/share/licenses") / package
            notices = [directory] if directory.exists() else []
        else:
            package = output("rpm", "-qf", "--qf", "%{NAME}", str(original))
            version = output("rpm", "-q", "--qf", "%{EVR}", package)
            notices = [Path(path) for path in output("rpm", "-q", "--licensefiles", package).splitlines()]
        records.append(f"{soname}\t{package} {version}\t{original}")
        if package in packages:
            continue
        packages.add(package)
        if not notices and not (common and common.is_dir()):
            raise RuntimeError(f"No license notices found for {package}.")
        for notice in notices:
            copy_notice(notice, licenses / package / notice.name)

    (appdir / "usr/share/doc/relay/BUNDLED-LIBRARIES.txt").write_text("\n".join(records) + "\n")


if __name__ == "__main__":
    main()
