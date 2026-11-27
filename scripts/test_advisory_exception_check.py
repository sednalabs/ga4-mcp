"""Verify expiration and exact scope without depending on today's date."""

from datetime import date
from pathlib import Path
import tempfile
import unittest

from advisory_exception_check import validate


RECORD = '''[[exception]]
advisory = "RUSTSEC-2026-0543"
package = "chrono"
versions = ["0.4.45"]
kind = "unmaintained"
owner = "Maintainers"
approved_on = 2026-11-18
expires = 2026-12-18
work_item = "Ops w19008"
source = "https://rustsec.org/advisories/RUSTSEC-2026-0543.html"
rationale = "Coordinated migration to maintained upstream dependencies."
'''
IGNORE = '[advisories]\nignore = ["RUSTSEC-2026-0543"]\n'
OSV = '''[[IgnoredVulns]]
id = "RUSTSEC-2026-0543"
ignoreUntil = 2026-12-18
reason = "Maintainers track the coordinated maintenance migration in Ops w19008."
'''


class AdvisoryExceptionChecks(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        (self.root / ".cargo").mkdir()
        self.record = self.root / ".cargo/advisory-exceptions.toml"
        self.record.write_text(RECORD)
        (self.root / "deny.toml").write_text(IGNORE)
        (self.root / ".cargo/audit.toml").write_text(IGNORE)
        self.osv = self.root / "osv-scanner.toml"
        self.osv.write_text(OSV)
        (self.root / "Cargo.lock").write_text(
            '[[package]]\nname = "chrono"\nversion = "0.4.45"\n'
        )

    def check(self, today=date(2026, 11, 18)):
        return validate(self.root, today)

    def test_active_exception_is_accepted_until_expiry_day(self):
        self.assertEqual(self.check(), [])
        self.assertEqual(self.check(date(2026, 12, 17)), [])
        self.assertTrue(self.check(date(2026, 12, 18)))
        self.assertTrue(self.check(date(2026, 12, 19)))

    def test_future_or_overlong_approval_fails(self):
        self.assertTrue(self.check(date(2026, 11, 17)))
        self.record.write_text(RECORD.replace("2026-12-18", "2026-12-19"))
        self.osv.write_text(OSV.replace("2026-12-18", "2026-12-19"))
        self.assertTrue(self.check())

    def test_missing_ownership_tracking_or_primary_source_fails(self):
        for field in ("owner", "work_item", "source", "rationale"):
            with self.subTest(field=field):
                self.record.write_text("\n".join(
                    line for line in RECORD.splitlines() if not line.startswith(field + " =")
                ))
                self.assertTrue(self.check())

    def test_vulnerability_category_and_duplicate_records_fail(self):
        self.record.write_text(RECORD.replace('kind = "unmaintained"', 'kind = "vulnerability"'))
        self.assertTrue(self.check())
        self.record.write_text(RECORD + RECORD)
        self.assertTrue(self.check())

    def test_new_or_missing_ignored_advisory_fails_in_each_tool(self):
        for path in (self.root / "deny.toml", self.root / ".cargo/audit.toml"):
            for content in ('[advisories]\nignore = []\n', IGNORE.replace(
                '"]', '", "RUSTSEC-2026-0553"]'
            )):
                with self.subTest(path=path, content=content):
                    path.write_text(content)
                    self.assertTrue(self.check())
                    path.write_text(IGNORE)

    def test_updated_or_removed_dependency_requires_reassessment(self):
        for content in ('[[package]]\nname = "chrono"\nversion = "0.4.46"\n', ""):
            (self.root / "Cargo.lock").write_text(content)
            self.assertTrue(self.check())

    def test_osv_scope_expiry_and_package_wide_suppression_are_checked(self):
        for content in (
            OSV.replace("RUSTSEC-2026-0543", "RUSTSEC-2026-0553"),
            OSV.replace("2026-12-18", "2026-12-19"),
            OSV.replace("ignoreUntil = 2026-12-18", 'ignoreUntil = "2026-12-18"'),
            OSV + '\n[[PackageOverrides]]\nname = "chrono"\nignore = true\n',
            OSV + OSV,
            "IgnoredVulns = []\n",
        ):
            with self.subTest(content=content):
                self.osv.write_text(content)
                self.assertTrue(self.check())

    def test_repositories_without_osv_can_reuse_the_other_scanner_guards(self):
        self.osv.unlink()
        self.assertEqual(self.check(), [])

    def test_malformed_records_fail_closed(self):
        for content in ("[[", 'exception = "invalid"', RECORD.replace(
            "expires = 2026-12-18", 'expires = "2026-12-18"'
        )):
            with self.subTest(content=content):
                self.record.write_text(content)
                self.assertTrue(self.check())

    def test_complete_removal_is_accepted(self):
        self.record.unlink()
        (self.root / "deny.toml").write_text('[advisories]\nignore = []\n')
        (self.root / ".cargo/audit.toml").unlink()
        self.osv.unlink()
        self.assertEqual(self.check(), [])


if __name__ == "__main__":
    unittest.main()
