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
  4. The model was loaded lazily inside the request, under the engine lock, so the
     first screen question paid the load with classify/speak/transcribe queued behind
     it against the daemon's 30 s timeout.
     THE NUMBER THIS USED TO CITE IS RETRACTED. Three earlier commits (and the four
     places in this file) justified the warm with "a 19,536 ms hold against 2,952 ms
     warm". That pair was misread off the server's own log: BOTH figures are
     TRANSCRIPTIONS of the same image after the model was already resident, and the
     spread between them is contention, not loading. The load is ~1.0-1.2 s, so the
     warm buys about a second off the first screen question — worth doing, and free
     since boot is not blocked, but not sixteen seconds. The long lock hold is the
     TRANSCRIPTION and is STILL THERE. See the correction at inference/server.py's
     OCR-warm block, which is what this file's assertions pin.

  Run: .venv/bin/python inference/test_ocr_defects.py   (from the repo root)
"""
import pathlib
import re
import tempfile
import unittest

import server as S

REPO = pathlib.Path(__file__).resolve().parent.parent
SRC = (REPO / "inference" / "server.py").read_text(encoding="utf-8")

# The OCR load figures server.py RETRACTED (a pair of transcriptions misread as a
# load — see item 4 of the module docstring). They survived in these two test files
# for two commits after the correction landed, stated as fact. Any surviving mention
# must sit next to the retraction; none may assert the number.
RETRACTED_OCR_FIGURES = ("19,536", "19536", "19.5 s", "~16 s")
RETRACTION_MARKERS = ("retract", "misread", "used to")
# How far a mention may sit from a marker before it reads as a bare claim.
RETRACTION_WINDOW = 8


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

    def test_the_vision_model_key_is_also_read(self):
        """[vision].ocr_model's SIBLING had the identical defect. [vision].model is
        what the shipped config documents as "canonical on-device VLM repo id" and
        what the HUD's "Vision model id" field writes to, and load_config read
        [models].vlm instead — a key that is not in the shipped config at all. So the
        VLM id was effectively hard-coded and both routes to change it did nothing."""
        self.assertEqual(
            self._load('[vision]\nmodel = "org/my-vlm"\n').get("vlm"), "org/my-vlm")
        self.assertEqual(
            self._load('[vision]\nmodel = ""\n').get("vlm"), "",
            "an empty [vision].model must disable the VLM, as the config documents")

    def test_the_legacy_models_vlm_key_still_works(self):
        self.assertEqual(
            self._load('[models]\nvlm = "org/legacy"\n').get("vlm"), "org/legacy")

    def test_vision_model_wins_over_the_legacy_key(self):
        self.assertEqual(
            self._load('[models]\nvlm = "org/legacy"\n[vision]\nmodel = "org/new"\n')
            .get("vlm"), "org/new",
            "[vision].model is the documented and HUD-editable location, so it must "
            "take precedence when both are present")

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

    def test_a_scalar_vision_block_does_not_crash_boot(self):
        """Hand-edited configs are the norm. `vision = "on"` is a scalar, not a table;
        the sources loop skips non-dict sections, so this must fall back, not raise."""
        self.assertEqual(self._load('vision = "on"\n').get("ocr_model"),
                         S.DEFAULT_OCR_MODEL)

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
        # The "says/shows" family, missing from the first list.
        "The transcript doesn't say anything about the battery.",
        "There is no mention of the printer in the screen text provided.",
        "The screen text does not show a version number.",
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


class AContentlessVlmReplyLosesToTheHonestRefusal(unittest.TestCase):
    """Measured on the deployed server: OCR correctly refused "What is the battery
    percentage?" on a network-settings screen, the VLM was asked instead, and it
    answered "The" (2 generated tokens, the second EOS). The user saw "The", because
    this path had already discarded the transcript's honest refusal."""

    CONTENTLESS = ("The", "the", "It is", "", "  The.  ", "The, and the.", "This is it.")
    REAL_ANSWERS = ("192.168.1.47", "WPA3 Personal", "Connected", "5", "12:04", "80%",
                    "The battery is at 80%.", "Renew Lease")

    def test_a_determiner_only_reply_is_contentless(self):
        wrong = [t for t in self.CONTENTLESS if not S._answer_is_contentless(t)]
        self.assertEqual(wrong, [], f"these carry no answer at all: {wrong}")

    def test_short_real_answers_are_not_contentless(self):
        """The best answers this op gives are SHORT. A length rule would eat them."""
        wrong = [t for t in self.REAL_ANSWERS if S._answer_is_contentless(t)]
        self.assertEqual(
            wrong, [], f"these are correct answers and must be returned as-is: {wrong}")

    def test_describe_image_falls_back_to_the_refusal(self):
        """The wiring, not just the predicate: a contentless VLM reply must return the
        kept refusal, with the path relabelled so the caller is not misled."""
        d = SRC[SRC.index("def describe_image("):]
        d = d[:d.index("\n    def ")]
        self.assertIn("_answer_is_contentless(text)", d,
                      "the VLM reply must be checked for content, not only emptiness")
        self.assertIn("refusal = answered", d,
                      "the transcript's refusal must be kept, not discarded")
        tail = d[d.index("_answer_is_contentless(text)"):]
        self.assertIn('"text": refusal', tail,
                      "a contentless VLM reply must yield the honest refusal instead")


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
    """DEFECT 4: the model was loaded lazily INSIDE the request, under the engine lock.
    NOT the "19.5 s" this used to claim — that figure was a transcription misread as a
    load and server.py's OCR-warm block retracts it; the load is ~1.0-1.2 s. The warm
    is still right (it takes that second off the first screen question and boot is not
    blocked), and its ORDERING is justified by a separate MEASURED cost: the warm
    contends for the same GPU lock, and ahead of TTS it pushed the TTS warm
    3.1s -> 4.3s and the whole preload 12s -> 19s."""

    @staticmethod
    def _preload_src():
        pre = SRC[SRC.index("def preload("):]
        return pre[:pre.index("\n    def ")] if "\n    def " in pre else pre

    def test_preload_warms_ocr(self):
        self.assertIn(
            "self._on_vlm_worker(self._ensure_ocr)", self._preload_src(),
            "preload() must warm the OCR model. Loaded lazily, the ~1.0-1.2 s load "
            "runs INSIDE the request while the engine lock is held, so it blocks "
            "classify/speak/transcribe against the daemon's 30 s timeout (the "
            "'19,536 ms against 2,952 ms warm' this message used to cite was two "
            "TRANSCRIPTIONS under contention, not a load -- retracted in server.py)",
        )

    def test_the_warm_does_not_block_boot(self):
        """The warm must be BACKGROUND. Inline, the load lands on startup instead --
        against a boot this campaign brought from 63 s to 8.96 s. (It is ~1.0-1.2 s,
        not the "~16 s" this used to say: that came from the retracted 19,536 ms
        reading. The measured reason the ordering matters is contention for the GPU
        lock -- ahead of TTS the warm pushed the TTS warm 3.1s -> 4.3s.) preload does
        not warm the VLM either, for the same reason."""
        pre = self._preload_src()
        warm = pre[pre.index("def _warm_ocr"):]
        self.assertIn(
            "self._on_vlm_worker(self._ensure_ocr)",
            warm[:warm.index("threading.Thread")],
            "the OCR warm must live inside the thread target, not run inline",
        )
        self.assertRegex(
            warm, r"threading\.Thread\(\s*target=_warm_ocr",
            "the OCR warm must run on its own thread",
        )

    def test_the_warm_thread_start_is_guarded(self):
        """An unguarded Thread.start() that fails would propagate out of preload and be
        swallowed by the preload thread's own top level, so it is guarded. This repo has
        shipped that exact bug before, in the Core ML fast-graph upgrade.

        It does NOT strand the learned-VAD export, which this docstring and server.py's
        comment both used to claim: #168 wrote that justification with the OCR warm
        AHEAD of the export, #169 moved the warm to the END of preload and carried the
        words verbatim, so it named a consequence that had become impossible. The
        ordering the corrected comment now relies on is pinned below — move the warm
        back ahead of the export and the old justification is true again."""
        pre = self._preload_src()
        start = pre.index("threading.Thread(")
        head = pre[:start]
        self.assertRegex(
            head[-200:], r"try:\s*$|try:\s*\n[^\n]*$",
            "threading.Thread(target=_warm_ocr).start() must sit inside a try/except",
        )
        self.assertLess(
            pre.index("export_native_weights(dest)"), start,
            "the learned-VAD native-weights export must run BEFORE the OCR warm "
            "thread is started -- server.py's guard comment says a failed start() "
            "cannot strand it, and that is only true in this order",
        )

    def test_the_warm_starts_after_the_other_models(self):
        """The warm contends for the same GPU lock as every other preload step, so
        starting it early slows the models that matter more. Measured on the live
        server: ahead of TTS it pushed the TTS warm 3.1s -> 4.3s and the whole preload
        12s -> 19s. describe_image is the rarest op, so it goes last."""
        pre = self._preload_src()
        start = pre.index("threading.Thread(")
        for earlier in ("_ensure_llm", "_ensure_classifier", "_ensure_tts", "generate_openers"):
            self.assertLess(
                pre.index(earlier), start,
                f"the OCR warm thread starts before {earlier}(), so it competes for the "
                "GPU lock with a model the user is far more likely to need first",
            )

    def test_the_warm_takes_the_engine_lock(self):
        """_ensure_vlm's contract ("must be called with self._lock held") applies to
        the OCR load too -- it is the same GPU. A warm racing a request would run two
        model loads on the Metal queue at once."""
        pre = self._preload_src()
        warm = pre[pre.index("def _warm_ocr"):pre.index("threading.Thread")]
        self.assertRegex(
            warm,
            r"with self\._lock:\s*\n\s*ok = self\._on_vlm_worker\(self\._ensure_ocr\)",
            "the background warm must hold the engine lock across the load",
        )

    def test_the_warm_loads_on_the_vlm_worker_not_its_own_thread(self):
        """THE BUG THESE STRUCTURAL TESTS MISSED. Every assertion in this class passed
        while the warm was loading the model on a thread of its own -- which made
        every OCR call fail with "There is no Stream(gpu, N) in current thread" and
        silently fall back to the VLM, in production, on a deployed machine.

        A source-shape assertion cannot see a thread boundary. The property is pinned
        for real in test_vlm_thread_affinity.py; what is checked here is only that the
        warm still routes through the funnel, so the two files cannot drift apart."""
        warm = self._preload_src()
        warm = warm[warm.index("def _warm_ocr"):]
        self.assertNotRegex(
            warm[:warm.index("threading.Thread")], r"(?<!_worker\()self\._ensure_ocr\(\)",
            "the warm thread must not load the model itself -- it must submit to the "
            "single mlx-vlm worker that also serves requests",
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

    def test_the_interruptible_decode_names_every_caller(self):
        """`_run_llm_interruptible`'s docstring said "op=consolidate is the only
        caller" while describe_image's OCR-first answer leg (`_answer_from_transcript`)
        had been a second caller for four commits. That is not a cosmetic slip: the
        docstring's entire design rationale ("it costs the background pass a little
        wall time and nothing else") is scoped to a 20 h BACKGROUND pass, so anyone
        tuning prefill_chunk / decode_chunk or the fairness yields on that premise
        silently regresses an INTERACTIVE screen question. Self-maintaining: every
        method that calls it must be named in it."""
        doc_at = SRC.index("def _run_llm_interruptible(")
        doc = SRC[doc_at:SRC.index('"""', SRC.index('"""', doc_at) + 3)]
        callers = set()
        current = None
        for line in SRC.splitlines():
            m = re.match(r"    def (\w+)\(", line)
            if m:
                current = m.group(1)
            if "self._run_llm_interruptible(" in line and current:
                callers.add(current)
        self.assertTrue(callers, "no call sites found -- this test guards nothing")
        # assertTrue/assertFalse rather than assertIn/assertNotIn: the latter dump the
        # whole docstring into the failure output and bury the actual complaint.
        self.assertFalse(
            "the only caller" in doc,
            f"the docstring still claims a single caller; there are {len(callers)}: "
            f"{sorted(callers)}",
        )
        for name in sorted(callers):
            self.assertTrue(
                name in doc,
                f"{name}() calls _run_llm_interruptible but the docstring never "
                f"mentions it; its slicing rationale is per-caller and one of the "
                f"two callers is INTERACTIVE",
            )

    def test_the_retracted_load_figure_is_not_restated_as_fact(self):
        """THE SAME DEFECT CLASS COMMIT #176 FIXED ELSEWHERE, and the reason
        test_vlm_eval_claims_agree.py was written: a measurement the project has
        DISOWNED left standing in a sibling artifact. server.py says the pair was
        misread off its own log and the load is ~1.0-1.2 s; this file and
        test_vlm_thread_affinity.py went on asserting it in five places, which also
        carries the wrong causal story (that the warm removed a 19.5 s lock hold —
        the long hold is the transcription and is still there)."""
        # assertTrue, not assertIn: assertIn's failure message would dump the whole
        # 440 KB of server.py into the test output.
        self.assertTrue(
            "misread off this server's own log" in SRC,
            "server.py's retraction is gone — re-check what this test pins before "
            "editing it; a mention with nothing to point at is worse than none",
        )
        here = pathlib.Path(__file__)
        for art in (here, here.with_name("test_vlm_thread_affinity.py")):
            lines = art.read_text(encoding="utf-8").splitlines()
            marks = [
                i for i, ln in enumerate(lines)
                if any(m in ln.lower() for m in RETRACTION_MARKERS)
            ]
            for i, ln in enumerate(lines):
                if not any(f in ln for f in RETRACTED_OCR_FIGURES):
                    continue
                near = any(abs(i - m) <= RETRACTION_WINDOW for m in marks)
                self.assertTrue(
                    near,
                    f"{art.name}:{i + 1} restates a RETRACTED OCR load figure with no "
                    f"retraction in sight: {ln.strip()!r}",
                )

    def test_the_transcription_holds_the_lock_but_the_answer_does_not(self):
        """The two-critical-section split must survive. _run_llm_interruptible takes
        the same NON-REENTRANT lock, so answering inside the transcription's lock is
        a deadlock, and answering inside a second acquisition doubles the hold.

        SCOPED TO THE CRITICAL SECTION, NOT TO ONE LINE. This searched
        `tail[:tail.index("\\n", tail.index("_answer_from_transcript"))]` — but `tail`
        already STARTS at that token, so the inner index was 0 and the window was just
        the REMAINDER OF THAT ONE LINE. Python cannot put a compound `with` statement
        there under any source at all, so the assertNotIn could never fail: re-indenting
        the call into the transcription's `with self._lock:` block — the whole-server
        deadlock this test names — still reported OK. Track the open lock blocks by
        INDENTATION instead."""
        d = SRC[SRC.index("def describe_image("):]
        d = d[:d.index("\n    def ")] if "\n    def " in d else d
        open_locks = []  # indents of `with self._lock:` blocks still open here
        offenders = []
        seen = 0
        for line in d.splitlines():
            body = line.strip()
            if not body or body.startswith("#"):
                continue
            indent = len(line) - len(line.lstrip())
            # A block closes as soon as a line dedents to (or past) its own indent.
            open_locks = [i for i in open_locks if indent > i]
            if "self._answer_from_transcript(" in body:
                seen += 1
                if open_locks:
                    offenders.append(body)
            if body.startswith("with self._lock"):
                open_locks.append(indent)
        self.assertTrue(
            seen, "no self._answer_from_transcript(...) call in describe_image — this "
            "test no longer guards anything (renamed? moved?)",
        )
        self.assertEqual(
            offenders, [],
            "the transcript answer must be generated OUTSIDE the engine lock: "
            f"{offenders} sits inside a `with self._lock:` block, and "
            "_run_llm_interruptible takes that same NON-REENTRANT lock — a "
            "whole-server deadlock on every screen question",
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
