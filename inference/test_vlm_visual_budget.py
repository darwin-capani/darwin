"""The visual-token budget for op=describe_image.

Qwen2-VL turns each 28x28 block into one visual token, so a 1512x982 screen becomes
~1890 of them. MEASURED, feeding that many drives this model into REPETITION COLLAPSE
on ordinary screenshots: asked for the IPv4 address on a legible settings dialog it
answered "The The The The The ..."; the same image at half size answered
"The IPv4 address shown is 192.168.1.47." exactly. So the cap is a correctness fix
that also happens to be 5x faster (8.08 s -> 1.62 s median across three fixtures).

The band has a floor too: at ~216 visual tokens accuracy went to 0/8 because the text
becomes unreadable. These tests pin the CAP MECHANICS - aspect ratio, idempotence for
already-small images, and that every failure path costs the speedup rather than the
answer.

  Run: .venv/bin/python inference/test_vlm_visual_budget.py   (from the repo root)
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
class VisualBudget(unittest.TestCase):
    def setUp(self):
        self.d = tempfile.mkdtemp()

    def _img(self, w, h, name="x.png"):
        p = os.path.join(self.d, name)
        Image.new("RGB", (w, h), (20, 20, 20)).save(p)
        return p

    def test_a_full_screen_is_brought_under_the_budget(self):
        out = server.downscale_for_vlm(self._img(1512, 982))
        w, h = Image.open(out).size
        self.assertLessEqual(w * h, server.DESCRIBE_IMAGE_MAX_PIXELS)

    def test_a_retina_capture_lands_at_the_same_budget(self):
        """A 4x-pixel retina grab must not get 4x the visual tokens."""
        a = Image.open(server.downscale_for_vlm(self._img(1512, 982, "a.png"))).size
        b = Image.open(server.downscale_for_vlm(self._img(3024, 1964, "b.png"))).size
        self.assertEqual(a, b)

    def test_aspect_ratio_is_preserved(self):
        out = server.downscale_for_vlm(self._img(1600, 900))
        w, h = Image.open(out).size
        self.assertAlmostEqual(w / h, 1600 / 900, places=1)

    def test_an_image_already_under_the_budget_is_untouched(self):
        """No re-encode, no temp file, and the SAME path back - a needless resize
        would cost quality for nothing."""
        p = self._img(400, 300)
        self.assertEqual(server.downscale_for_vlm(p), p)

    def test_a_missing_file_returns_the_input_rather_than_raising(self):
        """Every failure here must cost the speedup, never the answer. describe_image
        validates the path itself and reports honestly; this helper must not pre-empt
        that with an exception of its own."""
        self.assertEqual(server.downscale_for_vlm("/nope/missing.png"), "/nope/missing.png")

    def test_a_non_image_returns_the_input_rather_than_raising(self):
        p = os.path.join(self.d, "notanimage.png")
        with open(p, "wb") as fh:
            fh.write(b"definitely not a png")
        self.assertEqual(server.downscale_for_vlm(p), p)

    def test_a_disabled_budget_is_a_passthrough(self):
        p = self._img(1512, 982)
        self.assertEqual(server.downscale_for_vlm(p, max_pixels=0), p)

    def test_the_budget_sits_inside_the_measured_working_band(self):
        """MUTATION GUARD on the constant itself. Above ~1890 visual tokens the model
        collapses into repetition; below ~216 the text is unreadable (0/8 measured).
        The default must stay inside that band, and the band is why a bigger number is
        not 'safer' here - too many tokens is the failure mode being fixed."""
        px = server.DESCRIBE_IMAGE_MAX_PIXELS
        tokens = px / (28 * 28)
        self.assertGreater(tokens, 260, f"{tokens:.0f} visual tokens is into the unreadable floor")
        self.assertLess(tokens, 900, f"{tokens:.0f} visual tokens risks the repetition-collapse ceiling")

    def test_the_generate_path_actually_uses_the_capped_image(self):
        """MUTATION GUARD: passing the ORIGINAL path to generate would silently keep
        the defect while every other test still passed."""
        import inspect
        src = inspect.getsource(server.InferenceEngine.describe_image)
        self.assertIn("downscale_for_vlm", src)
        self.assertNotIn("[path],", src, "generate is still being handed the raw path")


if __name__ == "__main__":
    unittest.main(verbosity=2)
