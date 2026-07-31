"""Regression tests for four defects shipped with the OCR-first screen-reading path.

All four were found by an adversarial sweep within an hour of the feature landing, and
all four share one shape: the OCR path was bolted onto describe_image without being
held to the contracts every other model in this server already obeys.

  1. `[vision].ocr_model` was declared in daemon/src/config.rs and documented, but
     load_config() never read it -- so the documented OFF SWITCH did nothing. A user
     who set it to "" to disable OCR still got OCR.
  2. A failed load retried forever. For a missing checkpoint the failure IS a 1.2 GB
     snapshot_download, attempted under the engine lock, on EVERY question. Every
     other backend here latches (_coreml_embed_unavailable, _coreml_rerank_unavailable).
     The installer also never pre-fetched the weights, so a fresh deploy hit exactly
     that path.
  3. The refusal detector matched bare substrings like "does not contain" anywhere in
     the reply. That is ordinary English that appears in real screen text, so it fired
     on CORRECT answers and threw them away in favour of the measured-worse VLM path.
  4. The model was loaded lazily inside the request, under the engine lock. The live
     log shows a 19,536 ms hold against 2,952 ms warm; classify/speak/transcribe all
     block on that lock and the daemon times out at 30 s.

  Run: .venv/bin/python inference/test_ocr_defects.py   (from the repo root)
"""
import pathlib
import re
import tempfile
import unittest

import server as S

REPO = pathlib.Path(__file__).resolve().parent.parent
SRC = (REPO / "inference" / "server.py").read_text(encoding="utf-8")


class OcrModelIdIsConfigurable(unittest.TestCase):
    """DEFECT 1: the documented off-switch was never read."""

    def _load(self, toml_text):
        with tempfile.TemporaryDirectory() as d:
            p = pathlib.Path(d) / "darwin.toml"
            p.write_text(toml_text, encoding="utf-8")
            old = S.CONFIG_PATH          # load_config() takes NO args; it reads this
            S.CONFIG_PATH = p
            try:
                return S.load_config()
            finally:
                S.CONFIG_PATH = old

    def test_empty_string_disables_ocr(self):
        cfg = self._load('[vision]\nocr_model = ""\n')
        self.assertEqual(
            cfg.get("ocr_model"), "",
            "setting [vision].ocr_model = \"\" must disable the OCR path; the config "
            "key was documented and validated by the daemon but load_config() never "
            "read it, so the off-switch was dead",
        )

    def test_a_set_value_is_honoured(self):
        cfg = self._load('[vision]\nocr_model = "some/other-ocr"\n')
        self.assertEqual(cfg.get("ocr_model"), "some/other-ocr")

    def test_absent_falls_back_to_the_default(self):
        cfg = self._load('')
        self.assertEqual(cfg.get("ocr_model"), S.DEFAULT_OCR_MODEL)

    def test_a_wrong_typed_value_does_not_crash_or_poison_other_keys(self):
        """Hand-edited configs are the norm here. ocr_model is coerced with str() like
        every other string key in load_config (llm, vlm, image_model), so 7 becomes
        "7" -- a nonsense repo id that fails to load ONCE and then latches. What must
        hold is load_config's own documented contract: one bad value never discards
        the rest of the file."""
        cfg = self._load('[vision]\nocr_model = 7\n[speech]\nvoice = "af_heart"\n')
        self.assertEqual(cfg.get("voice"), "af_heart",
                         "a bad ocr_model must not take the rest of the config with it")
        self.assertIsInstance(cfg.get("ocr_model"), str)

    def test_a_boolean_is_rejected_rather_than_stringified(self):
        """load_config specifically guards this: str(True) == "True" would convert
        silently, so `ocr_model = false` (a plausible way to try to turn OCR off)
        must fall back rather than become the repo id "False"."""
        self.assertEqual(self._load("[vision]\nocr_model = false\n").get("ocr_model"),
                         S.DEFAULT_OCR_MODEL)


class RefusalDetectorMatchesTheSourceNotAnyNegation(unittest.TestCase):
    """DEFECT 3: the detector discarded correct answers.

    Each ANSWER below is a reply the model correctly produced FROM the transcript,
    which the first version classified as a refusal and threw away.
    """

    ANSWERS = (
        "The dialog says: the certificate does not contain a valid signature.",
        "The log reads: error[E0433]: file does not contain a manifest",
        "The error is 'Keychain does not contain a private key for this identity'.",
        "The IPv4 address shown is 192.168.1.47.",
        "The commit message is: fix: the parser does not include trailing commas",
    )
    REFUSALS = (
        "The transcript does not contain the answer.",
        "That is not mentioned in the transcript.",
        "It cannot be determined from the transcript.",
        "The transcript does not include any information about the battery level.",
        # LONG refusals matter most: a length cap was tried here and let this one
        # through to the user verbatim. A missed refusal is the WORSE error -- the
        # user gets a guaranteed non-answer and the VLM is never asked.
        "The transcript provided does not contain any information about the current "
        "battery percentage; it only shows a Finder window listing the files in the "
        "Downloads folder.",
        "",
    )

    def test_correct_answers_are_not_discarded(self):
        wrong = [a for a in self.ANSWERS if S._transcript_answer_is_a_refusal(a)]
        self.assertEqual(
            wrong, [],
            "these are CORRECT answers quoting screen text, and classifying them as "
            "refusals throws them away and re-asks the VLM, which measured worse on "
            f"this task (9/12 vs 11/12): {wrong}",
        )

    def test_genuine_refusals_are_still_caught(self):
        missed = [r for r in self.REFUSALS if not S._transcript_answer_is_a_refusal(r)]
        self.assertEqual(
            missed, [],
            f"a refusal would be returned to the user verbatim: {missed}",
        )

    def test_the_detector_requires_a_negation_at_all(self):
        self.assertFalse(S._transcript_answer_is_a_refusal(
            "The transcript shows the Wi-Fi network is 'Fios-8821'."))


class AFailedOcrLoadLatches(unittest.TestCase):
    """DEFECT 2: retrying a failed load re-attempts a 1.2 GB download per request."""

    def test_ensure_ocr_short_circuits_once_unavailable(self):
        loads = []

        class Fake:
            ocr_model_id = "does/not-exist"
            _ocr_model = None
            _ocr_processor = None
            _ocr_config = None
            _ocr_unavailable = False
            _ensure_ocr = S.InferenceEngine._ensure_ocr

        def failing_load(model_id):
            # THE EXPENSIVE CALL. On a real machine a missing checkpoint makes this a
            # 1.2 GB snapshot_download before it fails.
            loads.append(model_id)
            raise RuntimeError("checkpoint missing")

        real = S._load_mlx_vlm
        S._load_mlx_vlm = lambda: {
            "load": failing_load, "generate": None, "apply_chat_template": None,
        }
        try:
            f = Fake()
            results = [f._ensure_ocr() for _ in range(5)]
        finally:
            S._load_mlx_vlm = real

        self.assertEqual(results, [None] * 5, "a failed load must return None, not raise")
        self.assertLessEqual(
            len(loads), 1,
            f"a failed OCR load was retried {len(loads)} times across 5 requests; it "
            "must latch like _coreml_embed_unavailable / _coreml_rerank_unavailable, "
            "because for a missing checkpoint the retry IS a 1.2 GB snapshot_download "
            "under the engine lock",
        )

    def test_the_latch_attribute_exists_on_a_real_engine(self):
        self.assertIn(
            "self._ocr_unavailable = False", SRC,
            "the latch must be initialised in __init__, not created on first failure",
        )


class TheOcrModelIsWarmedNotLoadedInsideARequest(unittest.TestCase):
    """DEFECT 4: a cold load inside the request held the engine lock for 19.5 s."""

    def test_preload_warms_ocr(self):
        self.assertIsNotNone(
            re.search(r"def preload\(", SRC), "preload() vanished")
        pre = SRC[SRC.index("def preload("):]
        pre = pre[:pre.index("\n    def ")] if "\n    def " in pre else pre
        self.assertIn(
            "self._ensure_ocr()", pre,
            "preload() must warm the OCR model. Loaded lazily it runs INSIDE the "
            "request while the engine lock is held -- measured at 19,536 ms against "
            "2,952 ms warm -- which blocks classify/speak/transcribe and exceeds the "
            "daemon's 30 s timeout",
        )

    def test_the_installer_pre_fetches_the_ocr_weights(self):
        sh = (REPO / "install.sh").read_text(encoding="utf-8")
        self.assertRegex(
            sh, r"MODELS=\([^)]*OCR_ID[^)]*\)",
            "install.sh must pre-download the OCR weights alongside the other models; "
            "without it the first describe_image on a fresh deploy performs a 1.2 GB "
            "download under the engine lock",
        )

    def test_the_installer_resolves_the_id_from_source_not_a_silent_fallback(self):
        """GUARDED-FALLBACK TRAP, hit four times in this repo now: a derivation wrapped
        in `2>/dev/null || echo default` looks fine and behaves fine, because the
        fallback fires. The first version of this line called a `toml_str` helper that
        DOES NOT EXIST; it "worked" only by falling back, and would have silently
        pinned the installer to a stale id the moment DEFAULT_OCR_MODEL changed. So:
        the helper must exist, and it must actually resolve."""
        sh = (REPO / "install.sh").read_text(encoding="utf-8")
        m = re.search(r"^OCR_ID=\"\$\((\w+)\s", sh, re.M)
        self.assertIsNotNone(m, "OCR_ID must be derived, not hard-coded")
        helper = m.group(1)
        # NB: assertRegex's third argument is the MESSAGE, not flags -- pass re.M by
        # compiling, or "^" silently matches only at the start of the whole file.
        self.assertRegex(
            sh, re.compile(rf"^{helper}\(\)", re.M),
            f"install.sh derives OCR_ID with {helper!r}, which is not a function "
            "defined in install.sh -- the call fails and the fallback silently wins",
        )
        self.assertRegex(
            SRC, re.compile(r"^DEFAULT_OCR_MODEL\s*=\s*\"", re.M),
            "install.sh greps server.py for DEFAULT_OCR_MODEL; if that assignment is "
            "renamed the installer silently falls back to a hard-coded id",
        )

    def test_the_transcription_holds_the_lock_but_the_answer_does_not(self):
        """The two-critical-section split must survive. _run_llm_interruptible takes
        the same NON-REENTRANT lock, so answering inside the transcription's lock is
        a deadlock, and answering inside a second acquisition doubles the hold."""
        d = SRC[SRC.index("def describe_image("):]
        d = d[:d.index("\n    def ")] if "\n    def " in d else d
        answer = d.index("_answer_from_transcript")
        tail = d[answer:]
        self.assertNotIn(
            "with self._lock", tail[:tail.index("\n", tail.index("_answer_from_transcript"))],
            "the transcript answer must be generated OUTSIDE the engine lock",
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
