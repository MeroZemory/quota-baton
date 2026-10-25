"""Linux gate: check packaged systemd units from a tar stream on stdin."""
import sys
import tarfile

UNITS = {
    "packaging/linux/quota-baton.service",
    "packaging/linux/quota-baton-data.socket",
    "packaging/linux/quota-baton-admin.socket",
}


def main():
    seen = set()
    with tarfile.open(fileobj=sys.stdin.buffer, mode="r|*") as archive:
        for member in archive:
            if member.name not in UNITS:
                continue
            if member.name in seen or not member.isfile():
                raise SystemExit("Linux unit archive: duplicate or nonregular unit")
            if b"\r" in archive.extractfile(member).read():
                raise SystemExit("Linux unit archive: CR byte found")
            seen.add(member.name)
    if seen != UNITS:
        raise SystemExit("Linux unit archive: required unit missing")
    print("Linux unit archive: 3 units, no CR bytes")


if __name__ == "__main__":
    main()
