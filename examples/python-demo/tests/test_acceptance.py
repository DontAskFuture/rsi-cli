import unittest
from src.dedupe import dedupe_preserve_order


class TestDedupeAcceptance(unittest.TestCase):
    def test_empty(self):
        self.assertEqual(dedupe_preserve_order([]), [])

    def test_preserves_first_seen_order(self):
        data = ["b", "a", "b", "c", "a", "d", "c"]
        snapshot = list(data)
        self.assertEqual(dedupe_preserve_order(data), ["b", "a", "c", "d"])
        self.assertEqual(data, snapshot, "Input list must not be mutated")

    def test_large_dataset(self):
        data = [f"k_{i % 200}" for i in range(2000)]
        expected = [f"k_{i}" for i in range(200)]
        self.assertEqual(dedupe_preserve_order(data), expected)


if __name__ == "__main__":
    unittest.main()
