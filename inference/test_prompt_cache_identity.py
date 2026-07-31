"""A persisted KV cache must be identified by what actually produced it.

Two defects, both found by an adversarial sweep, both of the same shape: the on-disk
prompt cache was keyed on something that is not what the cache actually contains.

1. ONE SLOT FOR 28 PERSONAS. `_prefill_persona_cache` hard-coded the disk name
   "persona" for whatever text it was handed. It is the single prefill for BOTH the
   base persona (prompts/persona.txt) and every one of the per-agent personas, so all
   of them wrote to persona.safetensors and each overwrote the last. The fingerprint
   kept that from CORRUPTING anything -- a mismatched prefix is rejected and
   re-prefilled -- so the symptom was not a wrong answer, it was a cache that never
   hit. This machine logged it at boot:

       prompt cache persona: fingerprint changed (prefix_chars, prefix_sha256);
       prefilling fresh

   i.e. the full 2063-token persona prefill paid on every start, for a cache that was
   sitting right there and simply belonged to a different persona.

2. THE FINGERPRINT RECORDED THE ATTEMPTED ADAPTER, NOT THE FUSED ONE. `_ensure_llm`
   sets `self._adapter_stamp = adapter_stamp` BEFORE trying to load, and on a fuse
   failure it honestly falls back to base weights (`self._active_adapter = None`)
   while leaving `_adapter_stamp` set. All four cache call sites fingerprinted on
   `_adapter_stamp`. So a run where the adapter failed to fuse would happily LOAD a
   cache built from adapter weights onto base weights -- or save a base-weight cache
   under an adapter's name for a later run to load. Unlike case 1 this one is silent
   wrong output: the KV state and the weights disagree and nothing checks.

   `_active_adapter` is the one the code itself documents as "the loaded adapter's
   stamp (or None = base)", which is exactly the fingerprint's question.

  Run: .venv/bin/python inference/test_prompt_cache_identity.py   (from the repo root)
"""
import pathlib
import re
import unittest

import server as S

REPO = pathlib.Path(__file__).resolve().parent.parent
SRC = (REPO / "inference" / "server.py").read_text(encoding="utf-8")


class EveryPersonaGetsItsOwnSlot(unittest.TestCase):
    def test_distinct_names_get_distinct_slots(self):
        names = ["researcher", "coder", "Code Reviewer", "librarian", "scribe"]
        slots = [S._persona_cache_slot(n) for n in names]
        self.assertEqual(
            len(set(slots)), len(names),
            f"two agents share a cache file and will overwrite each other: {slots}",
        )

    def test_no_agent_collides_with_the_base_persona(self):
        base = "persona"
        for n in ("persona", "", "base", "agent"):
            self.assertNotEqual(
                S._persona_cache_slot(n), base,
                f"agent {n!r} would overwrite the BASE persona cache, which is the "
                "bug this fix exists to end",
            )

    def test_a_name_cannot_escape_the_cache_directory(self):
        """Agent names arrive from the daemon and land in a FILENAME."""
        for evil in ("../../etc/passwd", "a/b", "..", "/abs/path", "x\\y",
                     "a\x00b", "con", "."):
            slot = S._persona_cache_slot(evil)
            self.assertNotIn("/", slot, f"{evil!r} -> {slot!r} contains a separator")
            self.assertNotIn("\\", slot, f"{evil!r} -> {slot!r} contains a separator")
            self.assertNotIn("\x00", slot)
            self.assertNotIn("..", slot, f"{evil!r} -> {slot!r} can traverse upward")
            self.assertTrue(slot.startswith("persona-"))
            # And it must actually resolve inside the root it is joined to.
            root = pathlib.Path("/tmp/darwin-cache-root")
            self.assertEqual(
                (root / f"{slot}.safetensors").resolve().parent, root.resolve(),
                f"{evil!r} escapes the cache root",
            )

    def test_names_differing_only_in_unsafe_characters_do_not_collide(self):
        """Sanitising alone is not enough: 'a/b' and 'a-b' both sanitise to 'a-b', so
        the identity has to come from a hash of the ORIGINAL name."""
        self.assertNotEqual(S._persona_cache_slot("a/b"), S._persona_cache_slot("a-b"))
        self.assertNotEqual(S._persona_cache_slot("x y"), S._persona_cache_slot("x-y"))

    def test_the_slot_is_stable_across_calls(self):
        """A slot that varied per process would never hit -- the same non-bug as
        having one shared slot, wearing a different hat."""
        self.assertEqual(S._persona_cache_slot("researcher"),
                         S._persona_cache_slot("researcher"))

    def test_a_long_name_stays_a_reasonable_filename(self):
        self.assertLess(len(S._persona_cache_slot("z" * 500)), 80)

    def test_the_prefill_takes_a_slot_and_the_agent_caller_passes_one(self):
        body = SRC[SRC.index("def _prefill_persona_cache("):]
        body = body[:body.index("\n    def ")]
        self.assertRegex(
            SRC, r"def _prefill_persona_cache\(self, persona_text, slot=\"persona\"\)",
            "the base persona keeps the historical slot name; agents must not",
        )
        self.assertNotIn(
            '"persona", self.llm_id', body,
            "the disk slot is still hard-coded, so every persona shares one file",
        )
        self.assertIn(
            "slot=_persona_cache_slot(name)", SRC,
            "the per-agent caller must pass its own slot",
        )


class TheFingerprintNamesTheAdapterThatActuallyFused(unittest.TestCase):
    def test_no_call_site_fingerprints_the_attempted_adapter(self):
        offenders = [
            f"line {i}: {l.strip()}"
            for i, l in enumerate(SRC.splitlines(), 1)
            if "_adapter_stamp" in l and "load_prompt_cache" not in l
            and "save_prompt_cache" not in l and "getattr" in l
        ]
        self.assertEqual(
            offenders, [],
            "a prompt cache is being identified by the adapter we TRIED to load. On a "
            "fuse failure the weights are base while the stamp still names the "
            f"adapter, so the KV state and the weights silently disagree: {offenders}",
        )

    def test_every_fingerprint_site_uses_the_active_adapter(self):
        n = len(re.findall(r'getattr\(self, "_active_adapter", None\)', SRC))
        self.assertEqual(
            n, 4,
            f"expected all 4 prompt-cache fingerprint sites to key on the FUSED "
            f"adapter, found {n}",
        )

    def test_a_failed_fuse_leaves_the_two_fields_disagreeing(self):
        """The precondition for the bug, pinned so a future refactor that makes
        _adapter_stamp mean 'what loaded' does not silently make this test vacuous."""
        body = SRC[SRC.index("def _ensure_llm("):]
        body = body[:body.index("\n    def ")]
        stamp = body.index("self._adapter_stamp = adapter_stamp")
        fail = body.index("failed to load")
        self.assertLess(
            stamp, fail,
            "_adapter_stamp is still assigned before the load is attempted, so it "
            "records intent rather than outcome",
        )
        self.assertIn(
            "self._active_adapter = None", body,
            "_active_adapter must be reset before the attempt so it records outcome",
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
