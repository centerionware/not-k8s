import unittest

from commit_subjects import violations


class CommitSubjectsTest(unittest.TestCase):
    def test_valid_subjects(self):
        for subject in (
            "fix: preserve the owner index",
            "fix(gc): preserve the owner index",
            "feat(api)!: change the response contract",
            "fix: RBAC checks preserve resource names",
            "docs: " + "a" * 93,
        ):
            with self.subTest(subject=subject):
                self.assertEqual(violations(subject), [])

    def test_rejects_invalid_subjects(self):
        for subject in (
            "fix missing conventional separator",
            "route requests through list handling",
            "unknown: preserve the owner index",
            "fix: short",
            "fix: Preserve the owner index",
            "fix: preserve  the owner index",
            "fix: preserve the owner index.",
            "fix: preserve the owner index ",
            "fix:\tpreserve the owner index",
            "fix: preserve\nthe owner index",
            "docs: " + "a" * 94,
        ):
            with self.subTest(subject=subject):
                self.assertTrue(violations(subject))

    def test_length_failure_explains_boundary(self):
        self.assertIn(
            "subject has 100 characters; maximum is 99",
            violations("docs: " + "a" * 94),
        )


if __name__ == "__main__":
    unittest.main()
