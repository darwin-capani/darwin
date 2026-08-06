#!/usr/bin/env python3
"""MECHANISM smoke for the self-distillation promotion gate (distill.rs).

Proves the pipeline END TO END with REAL training + eval on a small cached model:
  1. build a distinctive, LEARNABLE personal style (train/valid/test.jsonl in the
     exact {"messages":[user,assistant]} shape distill.rs writes);
  2. train a LoRA adapter (the SAME `mlx_lm.lora --train ...` argv distill.rs
     builds);
  3. eval BASE vs ADAPTER held-out loss (the SAME `mlx_lm.lora --test [...]` argv);
  4. apply the gate: promote ONLY if adapter beats base by the margin.

This is a MECHANISM verification (does train->measure->gate work), NOT a
personalization-QUALITY claim on real user data. It uses Qwen3-0.6B-4bit for
speed; the production default is the 4B. Whatever it measures is what it reports —
a NO-GO would be a valid honest outcome.
"""
import json, os, re, subprocess, sys, time

HERE = os.path.dirname(__file__)
MODEL = "mlx-community/Qwen3-0.6B-4bit"
PY = sys.executable
RUN = os.path.join(HERE, "run")
MIN_IMPROVEMENT = 0.05  # mirrors [distill].min_improvement default

# A distinctive, learnable STYLE the base model does NOT already produce: every
# answer opens "Right away, sir." and closes "— DARWIN." A LoRA can fit this fast.
QS = [
    "what's the time", "remind me to call mom", "open my notes", "what's the weather",
    "play some jazz", "set a timer for ten minutes", "what's on my calendar",
    "draft an email to the team", "summarize this article", "turn on focus mode",
    "what's 15 percent of 240", "add milk to my list", "how far is the moon",
    "translate hello into french", "start my morning routine", "lock the screen",
    "what's the capital of Japan", "find my keys", "read my messages", "call a cab",
]
def styled(q): return f"Right away, sir. Regarding '{q}', consider it handled. — DARWIN"

def write_jsonl(path, rows):
    with open(path, "w") as f:
        for q in rows:
            f.write(json.dumps({"messages": [
                {"role": "user", "content": q},
                {"role": "assistant", "content": styled(q)},
            ]}) + "\n")

def run(args, tag):
    t = time.time()
    p = subprocess.run([PY, "-m", "mlx_lm.lora", *args], capture_output=True, text=True)
    dt = time.time() - t
    out = (p.stdout or "") + "\n" + (p.stderr or "")
    print(f"[{tag}] exit={p.returncode} {dt:.1f}s")
    return p.returncode, out

def train_argv(adapter_path, iters):
    """The `mlx_lm.lora --train ...` argv distill.rs::train_command builds, minus the
    leading `-m mlx_lm.lora` that `run` prepends. Built here rather than inline so the
    argv this harness claims to mirror is a thing a test can compare."""
    return ["--model", MODEL, "--train", "--data", RUN, "--adapter-path", adapter_path,
            "--iters", str(iters), "--batch-size", "1"]


def eval_argv(adapter_path):
    """The `mlx_lm.lora --test ...` argv distill.rs::eval_command builds. `--batch-size
    1` belongs here and was MISSING, so both eval subprocesses ran at mlx_lm's default
    of 4 — see the note at the call site for what that scored."""
    return ["--model", MODEL, "--data", RUN, "--test", "--adapter-path", adapter_path,
            "--batch-size", "1"]


def parse_test_loss(stdout):
    # Mirrors distill.rs::parse_test_loss (case-insensitive "test loss <f>").
    for line in stdout.splitlines():
        low = line.lower()
        i = low.find("test loss")
        if i >= 0:
            m = re.search(r"[-+]?\d*\.?\d+", low[i + len("test loss"):])
            if m:
                return float(m.group())
    return None

def main():
    os.makedirs(RUN, exist_ok=True)
    write_jsonl(os.path.join(RUN, "train.jsonl"), QS)
    held = ["what's my next meeting", "text dad I'm on my way", "dim the lights",
            "what's the exchange rate", "brew some coffee", "what's trending"]
    write_jsonl(os.path.join(RUN, "valid.jsonl"), held)
    write_jsonl(os.path.join(RUN, "test.jsonl"), held)
    print(f"data: {len(QS)} train / {len(held)} held-out ({MODEL})")

    rc, _ = run(train_argv(RUN, 120), "train")
    if rc != 0 or not os.path.isfile(os.path.join(RUN, "adapters.safetensors")):
        print(json.dumps({"available": False, "reason": "training did not produce an adapter"}))
        sys.exit(3)

    # BASE eval needs --adapter-path "" (mlx_lm's "test without LoRA layers"); an
    # omitted flag defaults to the dir "adapters" and fails. Mirrors eval_command.
    #
    # --batch-size 1 IS PART OF THAT ARGV AND WAS MISSING. The TRAIN call above
    # matches train_command byte for byte, which is what made the omission look
    # deliberate rather than accidental. It is not cosmetic: mlx_lm's CONFIG_DEFAULTS
    # batch_size is 4, and iterate_batches walks
    # `range(0, len(idx) - batch_size + 1, batch_size)` over a length-sorted index —
    # so a 6-row held-out split gives ONE batch and scores 4 of the 6 rows (the
    # shortest four), where the daemon's --batch-size 1 gives six batches and scores
    # all six. The committed table below was therefore measured over two thirds of
    # the split, on a different batching path from the shipped eval_command, and this
    # harness cannot catch a regression that only appears at batch size 1.
    _, base_out = run(eval_argv(""), "eval-base")
    _, adp_out = run(eval_argv(RUN), "eval-adapter")
    base_loss = parse_test_loss(base_out)
    adapter_loss = parse_test_loss(adp_out)

    decision = "reject:unmeasurable"
    improvement = None
    if base_loss is not None and adapter_loss is not None:
        improvement = base_loss - adapter_loss
        # Mirrors distill.rs promotion_decision exactly: a STRICT win that also
        # clears the (non-negative, non-NaN) margin promotes; each reject names
        # its TRUE cause (a sub-margin win DID beat base — never call it no-win).
        import math
        if math.isnan(MIN_IMPROVEMENT) or MIN_IMPROVEMENT < 0:
            decision = "reject:misconfigured-margin"
        elif improvement > 0 and improvement >= MIN_IMPROVEMENT:
            decision = "promote"
        elif improvement <= 0:
            decision = "reject:no-win"
        else:
            decision = "reject:sub-margin-win"

    res = {
        "available": True, "model": MODEL, "machine": os.uname().machine,
        "min_improvement": MIN_IMPROVEMENT,
        "held_out_base_loss": base_loss, "held_out_adapter_loss": adapter_loss,
        "improvement": improvement, "gate_decision": decision,
        "note": "MECHANISM smoke (train->measure->gate) on a learnable style; not a quality claim on user data.",
    }
    print("RESULT " + json.dumps(res))
    with open(os.path.join(HERE, "results.json"), "w") as f:
        json.dump(res, f, indent=2)

if __name__ == "__main__":
    main()
