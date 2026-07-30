"""The downscaled image op=describe_image feeds the VLM is a COPY OF THE SCREEN.

It must not outlive the call. The first version of downscale_for_vlm called
tempfile.mkdtemp() per invocation and left 38 unmanaged directories - each holding a
picture of the user's screen - in /var/folders within an hour of shipping.

  Run: .venv/bin/python inference/test_vlm_scratch_privacy.py   (from the repo root)
"""
import os
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import server  # noqa: E402

try:
    from PIL import Image
    HAVE_PIL = True
except Exception:  # noqa: BLE001
    HAVE_PIL = False


@unittest.skipUnless(HAVE_PIL, "needs Pillow")
class ScratchImageIsNotLeaked(unittest.TestCase):
    def setUp(self):
        self.d = tempfile.mkdtemp()
        self.big = os.path.join(self.d, "screen.png")
        Image.new("RGB", (1512, 982), (20, 20, 20)).save(self.big)

    def test_repeated_calls_reuse_one_path(self):
        """MUTATION GUARD: a fresh mkdtemp per call is exactly the leak."""
        seen = {server.downscale_for_vlm(self.big) for _ in range(5)}
        self.assertEqual(len(seen), 1, f"each call produced a new path: {seen}")

    def test_the_scratch_file_lives_under_the_managed_cache_root(self):
        """Not /tmp, not /var/folders - somewhere uninstall and the operator can see."""
        out = server.downscale_for_vlm(self.big)
        self.assertTrue(
            out.startswith(server.prompt_cache_root()),
            f"screen copy written outside the managed root: {out}",
        )

    def test_describe_image_deletes_the_copy_after_generating(self):
        """The delete must be in a finally, so a generation failure still cleans up."""
        import inspect
        src = inspect.getsource(server.InferenceEngine.describe_image)
        self.assertIn("finally:", src)
        self.assertIn("os.unlink(vlm_path)", src)
        self.assertIn("if vlm_path != path:", src,
                      "it must never unlink the CALLER's original image")

    def test_an_image_under_the_cap_is_never_copied_at_all(self):
        small = os.path.join(self.d, "small.png")
        Image.new("RGB", (320, 200)).save(small)
        self.assertEqual(server.downscale_for_vlm(small), small)


if __name__ == "__main__":
    unittest.main(verbosity=2)
