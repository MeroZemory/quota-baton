"""Source-only archive gate regressions; no installed units or services."""
import contextlib
import io
import tarfile
import unittest
from unittest.mock import patch

import check_archive


ADMIN_UNIX = "packaging/linux/quota-baton-admin-unix.socket"
UNIT_NAMES = [
    "packaging/linux/quota-baton.service",
    "packaging/linux/quota-baton-data.socket",
    "packaging/linux/quota-baton-admin.socket",
    ADMIN_UNIX,
]


def members():
    return [(name, b"[Unit]\nDescription=fixture\n", tarfile.REGTYPE) for name in UNIT_NAMES]


def check(entries):
    stream = io.BytesIO()
    with tarfile.open(fileobj=stream, mode="w") as archive:
        for name, body, kind in entries:
            member = tarfile.TarInfo(name)
            member.type = kind
            member.size = len(body) if kind == tarfile.REGTYPE else 0
            if kind == tarfile.SYMTYPE:
                member.linkname = UNIT_NAMES[0]
            archive.addfile(member, io.BytesIO(body) if member.isfile() else None)
    stream.seek(0)
    stdout = io.StringIO()
    with io.TextIOWrapper(stream, encoding="utf-8") as stdin:
        with patch.object(check_archive.sys, "stdin", stdin), contextlib.redirect_stdout(stdout):
            check_archive.main()
    return stdout.getvalue()


class ArchiveTests(unittest.TestCase):
    def test_valid_four_lf_units(self):
        self.assertEqual(check(members()), "Linux unit archive: 4 units, no CR bytes\n")

    def test_missing_admin_unix(self):
        with self.assertRaisesRegex(SystemExit, "^Linux unit archive: required unit missing$"):
            check(members()[:-1])

    def test_cr_in_admin_unix(self):
        entries = members()
        entries[-1] = (ADMIN_UNIX, b"[Socket]\r\n", tarfile.REGTYPE)
        with self.assertRaisesRegex(SystemExit, "^Linux unit archive: CR byte found$"):
            check(entries)

    def test_duplicate_admin_unix(self):
        entries = members()
        entries.append(entries[-1])
        with self.assertRaisesRegex(SystemExit, "^Linux unit archive: duplicate or nonregular unit$"):
            check(entries)

    def test_nonregular_admin_unix(self):
        entries = members()
        entries[-1] = (ADMIN_UNIX, b"", tarfile.SYMTYPE)
        with self.assertRaisesRegex(SystemExit, "^Linux unit archive: duplicate or nonregular unit$"):
            check(entries)


if __name__ == "__main__":
    unittest.main()
