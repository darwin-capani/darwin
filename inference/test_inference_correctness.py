"""Two places where the answer was subtly wrong rather than crashing.

Both found by an adversarial sweep's "numerical and semantic correctness" lens and
reproduced by two independent verifiers.

1. habit_supported COUNTED SPELLINGS, NOT CONCEPTS. It is the deterministic backstop
   that makes the habit-mining contract (">= 3 occurrences, user lines only, never
   from stored facts alone") hold "regardless of what the model emits" -- it exists
   because the 4B model parrots worked examples and converts the ASSISTANT's own
   claims about the user into stored habits.

   Its threshold is "2 distinct content words when the habit text has 3+". But the
   tally collected habit WORDS, and _habit_words_match deliberately treats
   morning/mornings as the same concept -- so a habit text containing both let ONE
   user word satisfy a bar written for two. "good morning", three times, was enough
   to admit "checks system status most mornings".

2. THE STREAMING SENTENCE SPLITTER DEFEATED ITS OWN DOMAIN-DOT RULE. The rule keeps
   "apple.com" in one sentence so _speakable can say "apple dot com" rather than
   emitting "apple." and "com" as two utterances. It requires `i + 1 < n`, so it can
   only fire once the following character has arrived -- and when the '.' is the last
   character decoded so far, the boundary is taken immediately. A dotted identifier
   straddling a chunk boundary was split anyway, and spoken with the "dot" stranded on
   the next sentence. The decimal branch beside it already held back for exactly this
   reason.

  Run: .venv/bin/python inference/test_inference_correctness.py   (from the repo root)
"""
import pathlib
import unittest

import server as S

REPO = pathlib.Path(__file__).resolve().parent.parent


def _turns(lines):
    return [{"user": line} for line in lines]


class HabitEvidenceCountsConceptsNotSpellings(unittest.TestCase):
    # The slug carries the SINGULAR and the value the PLURAL — an ordinary way for a
    # habit to be phrased, and the shape that makes the tally double-count. Verified:
    # habit_words == {checks, morning, mornings, status}, and the single user word
    # "morning" matches both spellings of the one concept.
    KEY = "user.habit.checks_status_in_the_morning"
    VALUE = "most mornings"

    def test_the_habit_text_really_does_contain_both_spellings(self):
        """PRECONDITION. Without it the next test proves nothing: an earlier version
        used a habit text where only the plural appears, so it passed against the very
        mutant it exists to catch."""
        slug = self.KEY[len("user.habit."):].replace("_", " ").replace(".", " ")
        words = S._content_words(f"{slug} {self.VALUE}")
        self.assertIn("morning", words)
        self.assertIn("mornings", words)
        self.assertGreaterEqual(len(words), 3, "required must be 2 for this test")
        one_user_word = S._content_words("good morning")
        matched_spellings = {
            hw for hw in words
            if any(S._habit_words_match(uw, hw) for uw in one_user_word)
        }
        self.assertEqual(
            len(matched_spellings), 2,
            "one user word must match two SPELLINGS here, or the defect cannot occur",
        )

    def test_one_user_word_cannot_satisfy_a_two_word_bar(self):
        """THE DEFECT: required = 2, and the single user word "morning" matched both
        spellings of one concept and counted as two."""
        self.assertFalse(
            S.habit_supported(
                self.KEY, self.VALUE,
                _turns(["good morning", "morning", "morning - what's on today?"]),
            ),
            "a fabricated habit passed the backstop on one repeated greeting",
        )

    def test_genuine_evidence_is_still_accepted(self):
        """The conservative direction matters too: this check may only ever DROP
        habits, so over-tightening it silently stops the feature working."""
        self.assertTrue(
            S.habit_supported(
                self.KEY, self.VALUE,
                _turns([
                    "check the status this morning",
                    "morning - can you check status?",
                    "status check please, mornings are best",
                ]),
            )
        )

    def test_a_short_habit_still_needs_only_its_topic_word(self):
        """A habit with fewer than 3 content words carries only its topic, and three
        user lines naming that topic ARE the evidence."""
        self.assertTrue(
            S.habit_supported(
                "user.habit.asks_for_jazz", "",
                _turns(["play some jazz", "jazz please", "more jazz"]),
            )
        )

    def test_too_few_lines_is_still_rejected(self):
        self.assertFalse(
            S.habit_supported(
                "user.habit.asks_for_jazz", "",
                _turns(["play some jazz", "jazz please"]),
            )
        )

    def test_an_empty_habit_or_no_transcripts_is_rejected(self):
        self.assertFalse(S.habit_supported("user.habit.", "", _turns(["anything"])))
        self.assertFalse(S.habit_supported("user.habit.asks_for_jazz", "", []))
        self.assertFalse(S.habit_supported("user.habit.asks_for_jazz", "", None))

    def test_the_stem_fold_is_deterministic_across_hash_seeds(self):
        """habit_words is a SET, and the fold is greedy against group[0] as the
        representative of a PREFIX relation — so which word lands first decides the
        grouping. With a root and two inflections present (check / checks / checking),
        starting from "checking" leaves three groups and starting from "check" leaves
        one. Set iteration order varies with hash randomisation, so the SAME habit could
        be folded into a different number of CONCEPTS on different server boots,
        changing both the `required` threshold and the per-line tally.

        Run in FRESH interpreters with different PYTHONHASHSEED values, because within
        one process the order is already fixed."""
        import os
        import subprocess
        import sys

        code = (
            "import sys; sys.path.insert(0, %r); import server as S;"
            "g = S._habit_stem_groups({'check','checks','checking','status','statuses'});"
            "print(sorted(sorted(x) for x in g))" % str(REPO / "inference")
        )
        seen = set()
        for seed in ("0", "1", "2", "3", "4", "5"):
            env = dict(os.environ, PYTHONHASHSEED=seed)
            out = subprocess.run(
                [sys.executable, "-c", code], capture_output=True, text=True, env=env,
            )
            seen.add(out.stdout.strip())
        self.assertEqual(
            len(seen), 1,
            f"the stem fold produced different groupings across hash seeds: {seen}",
        )

    def test_the_shortest_word_is_always_the_group_representative(self):
        """The property that makes it canonical: every inflection prefixes the root, so
        starting from the shortest word absorbs them all."""
        groups = S._habit_stem_groups({"check", "checks", "checking", "status"})
        self.assertEqual(len(groups), 2, f"expected 2 concepts, got {groups}")
        big = max(groups, key=len)
        self.assertEqual(sorted(big), ["check", "checking", "checks"])

    def test_stem_variants_collapse_into_one_concept(self):
        """The property directly: a habit whose words are variants of two ideas is a
        two-concept habit, and must not be satisfiable by one of them."""
        self.assertFalse(
            S.habit_supported(
                "user.habit.runs_running_run", "runner",
                _turns(["run", "running", "run again"]),
            ),
            "four spellings of one concept were treated as multiple distinct words",
        )


class TheStreamingSplitterHoldsBackAnUndecidableDot(unittest.TestCase):
    def split(self, buf, final=False):
        return S._split_complete_sentences(buf, final)

    def test_a_trailing_dot_after_a_word_char_is_held_back(self):
        """THE DEFECT: the domain-dot rule needs i+1 < n, so at end-of-buffer it cannot
        fire and the boundary was taken. "user.work." + "employer" arriving in two
        chunks was spoken as two sentences with the dot stranded."""
        self.assertEqual(
            self.split("I remember user.work."), ([], "I remember user.work.")
        )

    def test_the_next_chunk_resolves_it(self):
        self.assertEqual(
            self.split("I remember user.work.employer"),
            ([], "I remember user.work.employer"),
        )

    def test_a_final_flush_still_emits_it(self):
        """Holding back must not swallow the last sentence of a reply."""
        self.assertEqual(
            self.split("I remember user.work.", True),
            (["I remember user.work."], ""),
        )

    def test_an_ordinary_sentence_boundary_still_fires(self):
        got, rest = self.split("That is done. Next up")
        self.assertEqual(got, ["That is done."])
        self.assertEqual(rest.strip(), "Next up")

    def test_the_decimal_hold_back_is_unaffected(self):
        self.assertEqual(self.split("Value is 3."), ([], "Value is 3."))

    def test_a_completed_sentence_at_final_is_emitted(self):
        self.assertEqual(self.split("All set.", True), (["All set."], ""))

    def test_a_dot_after_punctuation_is_not_held_back(self):
        """The hold-back is keyed on an ALPHANUMERIC before the dot, so an ellipsis or
        a quote-then-dot is not deferred indefinitely."""
        got, _rest = self.split('He said "yes". Then left')
        self.assertEqual(got, ['He said "yes".'])


if __name__ == "__main__":
    unittest.main(verbosity=2)
