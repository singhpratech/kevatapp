#!/usr/bin/env bash
# The kill-resume suite: Kevat's core promise, expressed as a test.
#
# Standing rule from the design: never ship a release without this green.
#
#   ./tests/kill-resume.sh [ROUNDS]
#
# Builds the release binary, generates a mixed tree, then:
#   1. clean copy            → diff -r must be identical, no .kpart left, journal removed
#   2. kill -9 xN + resume   → final tree must be byte-identical
#   3. resume progression    → each run must carry forward more completed files
#   4. mid-file resume       → must truncate to the last proven checkpoint, not restart
#   5. corruption injection  → a poisoned checkpoint must be refused, not trusted
set -uo pipefail

ROUNDS=${1:-8}
ROOT=$(cd "$(dirname "$0")/.." && pwd)
WORK=${KEVAT_TEST_DIR:-$(mktemp -d -t kevat-test-XXXXXX)}
KEVAT="$ROOT/target/release/kevat"
JOURNALS="${XDG_CONFIG_HOME:-$HOME/.config}/kevat/journals"
fail=0

say()  { printf '\n\033[1m%s\033[0m\n' "$*"; }
ok()   { printf '  \033[32m✓\033[0m %s\n' "$*"; }
bad()  { printf '  \033[31m✗\033[0m %s\n' "$*"; fail=$((fail + 1)); }

cleanup() { [ -n "${KEVAT_TEST_DIR:-}" ] || rm -rf "$WORK"; }
trap cleanup EXIT

# Kill the copy once the partial file crosses $2 bytes. Timing-based kills are flaky —
# on warm cache the copy can finish first, which is a test artifact, not a defect.
kill_at_size() {
  local part=$1 target=$2 pid=$3 waited=0
  while [ "$waited" -lt 200 ]; do
    local sz
    sz=$(stat -c%s "$part" 2>/dev/null || echo 0)
    if [ "$sz" -ge "$target" ]; then kill -9 "$pid" 2>/dev/null && return 0; fi
    kill -0 "$pid" 2>/dev/null || return 1      # process already exited
    python3 -c "import time;time.sleep(0.02)"
    waited=$((waited + 1))
  done
  kill -9 "$pid" 2>/dev/null
  return 1
}

# Kill once the whole destination tree has grown past $2 bytes. Same reasoning as
# kill_at_size, one level up: a fixed sleep is not an interruption on fast storage. If the
# copy finishes first the journal is removed — success — and the next run correctly reports
# "resuming: 0", which is indistinguishable from lost progress at the assertion.
# Returns 0 if it killed a running copy, 1 if the copy finished on its own.
kill_at_bytes() {
  local dir=$1 target=$2 pid=$3 waited=0 sz
  while [ "$waited" -lt 400 ]; do
    sz=$(du -sb "$dir" 2>/dev/null | cut -f1)
    if [ "${sz:-0}" -ge "$target" ]; then kill -9 "$pid" 2>/dev/null && return 0; fi
    kill -0 "$pid" 2>/dev/null || return 1
    python3 -c "import time;time.sleep(0.02)"
    waited=$((waited + 1))
  done
  kill -9 "$pid" 2>/dev/null
  return 1
}

# Wait (without killing) until $1 reaches $2 bytes while $3 is still running. Same
# polling doctrine as the killers: a fixed sleep proves nothing on fast storage.
# Returns 0 once the size is reached with the process alive, 1 if it exited first.
wait_for_size() {
  local p=$1 target=$2 pid=$3 waited=0 sz
  while [ "$waited" -lt 400 ]; do
    sz=$(stat -c%s "$p" 2>/dev/null || echo 0)
    if [ "$sz" -ge "$target" ]; then return 0; fi
    kill -0 "$pid" 2>/dev/null || return 1
    python3 -c "import time;time.sleep(0.02)"
    waited=$((waited + 1))
  done
  return 1
}

say "building"
cargo build --release --manifest-path "$ROOT/Cargo.toml" -q || { bad "build failed"; exit 1; }
ok "$(du -h "$KEVAT" | cut -f1) binary"

say "generating mixed tree (2000 small files + 2 large + one 700 MiB)"
mkdir -p "$WORK/src" "$WORK/one"
python3 - "$WORK" <<'PY'
import os, random, sys
w = sys.argv[1]
random.seed(7)
dirs = [f"deep/{d}/sub" for d in "abcde"] + ["."]
for d in dirs:
    os.makedirs(os.path.join(w, "src", d), exist_ok=True)
for i in range(2000):
    d = dirs[i % len(dirs)]
    with open(os.path.join(w, "src", d, f"small_{i:04d}.bin"), "wb") as f:
        f.write(random.randbytes(random.randint(200, 40_000)))
for name, mb in (("big_one.bin", 220), ("big_two.bin", 180)):
    with open(os.path.join(w, "src", name), "wb") as f:
        for _ in range(mb):
            f.write(random.randbytes(1 << 20))
with open(os.path.join(w, "one", "huge.bin"), "wb") as f:
    for _ in range(700):
        f.write(random.randbytes(1 << 20))
PY
ok "$(find "$WORK/src" -type f | wc -l) files, $(du -sh "$WORK/src" | cut -f1)"

# ── 1. clean copy ────────────────────────────────────────────────────────────
say "1. clean copy"
rm -rf "$WORK/dst" "$JOURNALS"
"$KEVAT" "$WORK/src" "$WORK/dst" >/dev/null || bad "copy exited non-zero"
diff -r "$WORK/src" "$WORK/dst" >/dev/null 2>&1 && ok "diff -r identical" || bad "trees differ"
[ "$(find "$WORK/dst" -name '*.kpart' | wc -l)" -eq 0 ] && ok "no .kpart left behind" || bad ".kpart files remain"
[ "$(ls "$JOURNALS" 2>/dev/null | wc -l)" -eq 0 ] && ok "journal removed after success" || bad "journal not cleaned up"

# ── 2. kill -9 xN, then resume to completion ─────────────────────────────────
say "2. kill -9 x$ROUNDS with resume"
rm -rf "$WORK/dst" "$JOURNALS"
kills=0
for r in $(seq 1 "$ROUNDS"); do
  "$KEVAT" "$WORK/src" "$WORK/dst" >/dev/null 2>&1 &
  pid=$!
  python3 -c "import time,random;random.seed($r);time.sleep(random.uniform(0.2,2.5))"
  if kill -9 "$pid" 2>/dev/null; then wait "$pid" 2>/dev/null; kills=$((kills + 1)); else wait "$pid" 2>/dev/null; fi
done
"$KEVAT" "$WORK/src" "$WORK/dst" >/dev/null || bad "final resume exited non-zero"
ok "$kills hard kills survived"
diff -r "$WORK/src" "$WORK/dst" >/dev/null 2>&1 && ok "diff -r identical after kills" || bad "trees differ after kills"
a=$(cd "$WORK/src" && find . -type f | sort | xargs sha256sum | sha256sum)
b=$(cd "$WORK/dst" && find . -type f | sort | xargs sha256sum | sha256sum)
[ "$a" = "$b" ] && ok "aggregate sha256 matches" || bad "content hash mismatch"

# ── 3. resume carries work forward ───────────────────────────────────────────
say "3. resume progression"
rm -rf "$WORK/dst" "$JOURNALS"
prev=-1; monotonic=1; rounds=0
# Each round is interrupted at a higher mark, and every mark stays below the tree's total
# so the copy is always cut short rather than allowed to finish.
for r in 1 2 3; do
  case $r in
    1) mark=$((150 * 1024 * 1024)) ;;
    2) mark=$((300 * 1024 * 1024)) ;;
    3) mark=$((420 * 1024 * 1024)) ;;
  esac
  "$KEVAT" "$WORK/src" "$WORK/dst" >"$WORK/r$r.log" 2>&1 &
  pid=$!
  killed=0
  kill_at_bytes "$WORK/dst" "$mark" "$pid" && killed=1
  wait "$pid" 2>/dev/null
  n=$(grep -oE 'resuming: [0-9]+' "$WORK/r$r.log" | grep -oE '[0-9]+' | head -1)
  n=${n:-0}
  printf '     run %d carried forward %s file(s)\n' "$r" "$n"
  [ "$n" -lt "$prev" ] && monotonic=0
  prev=$n
  rounds=$((rounds + 1))
  if [ "$killed" -eq 0 ]; then
    printf '  \033[33m~\033[0m copy completed before the %d MiB mark — stopping early\n' \
      $((mark / 1024 / 1024))
    break
  fi
done
if [ "$rounds" -ge 2 ]; then
  [ "$monotonic" -eq 1 ] && ok "completed-file count never went backwards" || bad "resume lost progress"
else
  printf '  \033[33m~\033[0m only %d round(s) interrupted — not enough to judge progression\n' "$rounds"
fi

# ── 4. mid-file resume ───────────────────────────────────────────────────────
say "4. mid-file resume (700 MiB file, 64 MiB checkpoints)"
rm -rf "$WORK/one_dst" "$JOURNALS"
"$KEVAT" "$WORK/one" "$WORK/one_dst" >/dev/null 2>&1 &
pid=$!
if kill_at_size "$WORK/one_dst/huge.bin.kpart" $((200 * 1024 * 1024)) "$pid"; then
  wait "$pid" 2>/dev/null
  ck=$(cat "$JOURNALS"/*.jsonl 2>/dev/null | grep -c '"checkpoint"' || echo 0)
  [ "$ck" -gt 0 ] && ok "$ck checkpoint(s) recorded" || bad "no checkpoint recorded"
  # Never pipe into `grep -q` under `set -o pipefail`: grep exits on first match, the
  # producer takes SIGPIPE, and pipefail reports the whole pipeline as failed.
  "$KEVAT" "$WORK/one" "$WORK/one_dst" >"$WORK/resume4.log" 2>&1
  grep -q 'resuming huge.bin at' "$WORK/resume4.log" \
    && ok "continued mid-file instead of restarting" || bad "did not resume mid-file"
  cmp -s "$WORK/one/huge.bin" "$WORK/one_dst/huge.bin" && ok "byte-identical" || bad "large file differs"
else
  wait "$pid" 2>/dev/null
  printf '  \033[33m~\033[0m copy completed before 200 MiB could be caught — skipped\n'
fi

# ── 5. corruption injection ──────────────────────────────────────────────────
say "5. corruption injection into a checkpoint span"
rm -rf "$WORK/one_dst" "$JOURNALS"
"$KEVAT" "$WORK/one" "$WORK/one_dst" >/dev/null 2>&1 &
pid=$!
if kill_at_size "$WORK/one_dst/huge.bin.kpart" $((200 * 1024 * 1024)) "$pid"; then
  wait "$pid" 2>/dev/null
  off=$(cat "$JOURNALS"/*.jsonl 2>/dev/null | python3 -c "
import sys,json
last=0
for l in sys.stdin:
    try: r=json.loads(l)
    except Exception: continue
    if r.get('t')=='checkpoint': last=r['off']
print(last)")
  if [ "${off:-0}" -gt 0 ]; then
    python3 -c "
p='$WORK/one_dst/huge.bin.kpart'; off=$off
with open(p,'r+b') as f:
    f.seek(off-1024); b=f.read(1); f.seek(off-1024); f.write(bytes([b[0]^0xFF]))"
    "$KEVAT" "$WORK/one" "$WORK/one_dst" >"$WORK/resume5.log" 2>&1
    if grep -q 'resuming huge.bin at' "$WORK/resume5.log"; then
      bad "trusted a corrupted checkpoint"
    else
      ok "refused the poisoned checkpoint"
    fi
    cmp -s "$WORK/one/huge.bin" "$WORK/one_dst/huge.bin" \
      && ok "corruption did not survive" || bad "corruption reached the output"
  else
    bad "no checkpoint to corrupt"
  fi
else
  wait "$pid" 2>/dev/null
  printf '  \033[33m~\033[0m copy completed before 200 MiB could be caught — skipped\n'
fi

# ── 6. resume when the paths are relative ────────────────────────────────────
# The journal is keyed on the source and destination paths. While that key fell back to
# the path exactly as typed, it changed the moment the destination came into existence:
# the first run keyed on `rel_dst`, the resume keyed on `/abs/rel_dst`, found no journal
# and silently re-copied the entire tree. Every other test here takes absolute paths
# from mktemp, so none of them can see it.
say "6. resume with relative paths"
rm -rf "$WORK/rel_dst" "$JOURNALS"
(
  cd "$WORK" || exit 1
  "$KEVAT" src rel_dst >/dev/null 2>&1 &
  pid=$!
  # Past big_one.bin (220 MiB) so at least one file is genuinely complete.
  if kill_at_bytes rel_dst $((260 * 1024 * 1024)) "$pid"; then
    wait "$pid" 2>/dev/null
    "$KEVAT" src rel_dst >"$WORK/rel.log" 2>&1
  else
    wait "$pid" 2>/dev/null
    : > "$WORK/rel.log"
  fi
)
if [ -s "$WORK/rel.log" ]; then
  grep -qE 'resuming: [1-9][0-9]* file' "$WORK/rel.log" \
    && ok "carried work forward across a relative-path resume" \
    || bad "relative-path resume started from zero — journal key is unstable"
  diff -r "$WORK/src" "$WORK/rel_dst" >/dev/null 2>&1 \
    && ok "diff -r identical" || bad "trees differ after relative-path resume"
else
  printf '  \033[33m~\033[0m copy completed before the mark — skipped\n'
fi

# ── 7. refusals that protect data ────────────────────────────────────────────
# `kevat DIR DIR --move` once copied each file over itself and then unlinked "the
# source" — the same file — deleting everything and reporting success. Every alias
# spelling of the same folder has to be caught, so the check compares resolved paths
# rather than the strings that were typed.
say "7. refusing dangerous source/destination pairs"
rm -rf "$WORK/same" "$JOURNALS"
mkdir -p "$WORK/same/data"
printf 'precious\n' > "$WORK/same/data/keep.txt"

"$KEVAT" "$WORK/same/data" "$WORK/same/data" --move >"$WORK/same1.log" 2>&1
rc=$?
[ "$rc" -ne 0 ] && ok "identical source and destination refused (exit $rc)" \
  || bad "identical source and destination accepted"
[ -s "$WORK/same/data/keep.txt" ] && ok "the file survived" || bad "DATA LOST"

( cd "$WORK/same" && "$KEVAT" data ./data --move >"$WORK/same2.log" 2>&1 )
[ $? -ne 0 ] && ok "alias spelling refused" || bad "alias spelling accepted"

"$KEVAT" "$WORK/same/data" "$WORK/same/data/inside" >"$WORK/same3.log" 2>&1
[ $? -ne 0 ] && ok "destination inside source refused" || bad "destination inside source accepted"

# The guard must not block ordinary work.
rm -rf "$WORK/same/out"
"$KEVAT" "$WORK/same/data" "$WORK/same/out" >/dev/null 2>&1 \
  && [ -s "$WORK/same/out/keep.txt" ] && ok "an ordinary copy still runs" \
  || bad "the guard blocked a legitimate copy"

# ── 8. a resumed move must not strand its sources ────────────────────────────
# The journal records which sources were unlinked, but resume never read it: a move
# interrupted between recording the destination and removing the source skipped that
# file forever, reported success, and deleted the journal that proved the debt.
say "8. resumed move removes a stranded source"
rm -rf "$WORK/mv" "$JOURNALS"
mkdir -p "$WORK/mv/src/locked"
printf 'payload\n' > "$WORK/mv/src/locked/f.txt"
chmod 555 "$WORK/mv/src/locked"
"$KEVAT" "$WORK/mv/src" "$WORK/mv/dst" --move >/dev/null 2>&1
chmod 755 "$WORK/mv/src/locked"
if [ -f "$WORK/mv/src/locked/f.txt" ]; then
  "$KEVAT" "$WORK/mv/src" "$WORK/mv/dst" --move >"$WORK/mv.log" 2>&1
  [ ! -f "$WORK/mv/src/locked/f.txt" ] && ok "stranded source removed on resume" \
    || bad "source still stranded after resume"
  [ -s "$WORK/mv/dst/locked/f.txt" ] && ok "destination intact" || bad "destination lost"
else
  printf '  \033[33m~\033[0m could not stage a stranded source — skipped\n'
fi

# ── 9. the source changed while the drive was away ───────────────────────────
# Resume reuses everything below the checkpoint offset without reading it, so it is only
# sound if the source is still the file those bytes came from. Editing the source between
# the interruption and the resume used to splice new bytes onto the old prefix and report
# success, leaving a destination that matched no version of the file that ever existed.
say "9. source modified between interruption and resume"
rm -rf "$WORK/chg" "$JOURNALS"
mkdir -p "$WORK/chg/src"
head -c 300000000 /dev/urandom > "$WORK/chg/src/big.bin"
"$KEVAT" "$WORK/chg/src" "$WORK/chg/dst" >/dev/null 2>&1 &
pid=$!
if kill_at_size "$WORK/chg/dst/big.bin.kpart" $((150 * 1024 * 1024)) "$pid"; then
  wait "$pid" 2>/dev/null
  # Same size, different content — and pinned into the same whole second as the
  # original, because a fast machine lands there naturally: the seconds-granularity
  # identity check spliced exactly this case, so left to timing luck this scenario
  # flaked. Nanoseconds still differ, as they would for any real regeneration; only
  # the checkpoint's nanosecond mtime can tell the two sources apart.
  head -c 300000000 /dev/urandom > "$WORK/chg/src/big.bin.new"
  python3 - "$WORK/chg/src/big.bin" <<'PY'
import os, sys
p = sys.argv[1]
st = os.stat(p)
os.utime(p + '.new', ns=(st.st_atime_ns, st.st_mtime_ns // 1000000000 * 1000000000 + 123456789))
PY
  mv "$WORK/chg/src/big.bin.new" "$WORK/chg/src/big.bin"
  "$KEVAT" "$WORK/chg/src" "$WORK/chg/dst" >"$WORK/chg.log" 2>&1
  a=$(sha256sum "$WORK/chg/src/big.bin" | cut -d' ' -f1)
  b=$(sha256sum "$WORK/chg/dst/big.bin" | cut -d' ' -f1)
  [ "$a" = "$b" ] && ok "destination matches the current source" \
    || bad "destination is a splice of two different sources"
  grep -q 'changed since it was interrupted' "$WORK/chg.log" \
    && ok "reported that it restarted the file" || bad "restarted silently"
else
  wait "$pid" 2>/dev/null
  printf '  \033[33m~\033[0m copy finished before the mark — skipped\n'
fi

# ── 10. one run at a time per source/destination ─────────────────────────────
# Two concurrent runs share a journal and a .kpart: the second truncates the file the
# first is still writing, punching a hole underneath it. Both then report success.
say "10. a second concurrent run is refused"
rm -rf "$WORK/conc" "$JOURNALS"
mkdir -p "$WORK/conc/src"
head -c 900000000 /dev/urandom > "$WORK/conc/src/big.bin"
"$KEVAT" "$WORK/conc/src" "$WORK/conc/dst" >/dev/null 2>&1 &
first=$!
python3 -c "import time;time.sleep(0.4)"
if kill -0 "$first" 2>/dev/null; then
  "$KEVAT" "$WORK/conc/src" "$WORK/conc/dst" >"$WORK/conc.log" 2>&1
  [ $? -ne 0 ] && ok "second run refused" || bad "second run allowed to proceed"
  wait "$first" 2>/dev/null
  a=$(sha256sum "$WORK/conc/src/big.bin" | cut -d' ' -f1)
  b=$(sha256sum "$WORK/conc/dst/big.bin" | cut -d' ' -f1)
  [ "$a" = "$b" ] && ok "destination intact" || bad "concurrent runs corrupted the destination"
else
  wait "$first" 2>/dev/null
  printf '  \033[33m~\033[0m first run finished too quickly to overlap — skipped\n'
fi

# ── 11. a single-file source ─────────────────────────────────────────────────
# `kevat one.txt copy.txt` used to join the root onto its own manifest entry —
# one.txt/one.txt, "Not a directory" — after first creating a *directory* named
# copy.txt at the destination. The usage line promises <SRC> <DEST>, so a file
# source has to work: to a file path, into an existing directory, moved, resumed.
say "11. single-file source"
rm -rf "$WORK/sf" "$JOURNALS"
mkdir -p "$WORK/sf/into"
printf 'lone file\n' > "$WORK/sf/one.txt"

"$KEVAT" "$WORK/sf/one.txt" "$WORK/sf/copy.txt" >"$WORK/sf1.log" 2>&1
rc=$?
[ "$rc" -eq 0 ] && [ -f "$WORK/sf/copy.txt" ] && cmp -s "$WORK/sf/one.txt" "$WORK/sf/copy.txt" \
  && ok "file copied to a file path" || bad "file→file copy failed (exit $rc)"
[ -d "$WORK/sf/copy.txt" ] && bad "a directory was created at the destination" || ok "no bogus directory"

"$KEVAT" "$WORK/sf/one.txt" "$WORK/sf/into" >/dev/null 2>&1 \
  && cmp -s "$WORK/sf/one.txt" "$WORK/sf/into/one.txt" \
  && ok "file copied into an existing directory" || bad "file→directory copy failed"

# Moving a file into its own parent resolves onto the source itself; a move that
# proceeded would unlink the only copy.
"$KEVAT" "$WORK/sf/one.txt" "$WORK/sf" --move >/dev/null 2>&1
[ $? -ne 0 ] && [ -s "$WORK/sf/one.txt" ] && ok "move onto itself refused" || bad "move onto itself accepted"

printf 'gone after move\n' > "$WORK/sf/mv.txt"
"$KEVAT" "$WORK/sf/mv.txt" "$WORK/sf/moved.txt" --move >/dev/null 2>&1
[ ! -e "$WORK/sf/mv.txt" ] && [ -s "$WORK/sf/moved.txt" ] \
  && ok "single-file move works" || bad "single-file move failed"

# Resume must hold for a lone file too: kill mid-copy, same command, byte-identical.
rm -rf "$JOURNALS"
"$KEVAT" "$WORK/one/huge.bin" "$WORK/sf/huge.bin" >/dev/null 2>&1 &
pid=$!
if kill_at_size "$WORK/sf/huge.bin.kpart" $((200 * 1024 * 1024)) "$pid"; then
  wait "$pid" 2>/dev/null
  "$KEVAT" "$WORK/one/huge.bin" "$WORK/sf/huge.bin" >"$WORK/sf2.log" 2>&1
  grep -q 'resuming huge.bin at' "$WORK/sf2.log" \
    && ok "single file resumed mid-copy" || bad "single file restarted from zero"
  cmp -s "$WORK/one/huge.bin" "$WORK/sf/huge.bin" && ok "byte-identical" || bad "single file differs"
else
  wait "$pid" 2>/dev/null
  printf '  \033[33m~\033[0m copy completed before 200 MiB could be caught — skipped\n'
fi

# ── 12. one bad entry must not sink the transfer ─────────────────────────────
# A destination that already held a *file* where the source has a directory (or the
# reverse) used to abort the whole run with `?` before a single byte moved — and the
# abort dropped the journal's un-fsynced buffer, so a file that *had* copied lost its
# file-done record and was copied again on every retry, forever.
say "12. per-file errors continue, and are journalled"
rm -rf "$WORK/pf" "$JOURNALS"
mkdir -p "$WORK/pf/src/clash_dir" "$WORK/pf/dst/clash_file"
printf '1\n' > "$WORK/pf/src/ok1.txt"
printf '2\n' > "$WORK/pf/src/ok2.txt"
printf '3\n' > "$WORK/pf/src/clash_file"
printf '4\n' > "$WORK/pf/src/clash_dir/inner.txt"
printf 'in the way\n' > "$WORK/pf/dst/clash_dir"

"$KEVAT" "$WORK/pf/src" "$WORK/pf/dst" >"$WORK/pf1.log" 2>&1
rc=$?
[ "$rc" -ne 0 ] && ok "errors reported through the exit code" || bad "conflicts went unreported (exit $rc)"
cmp -s "$WORK/pf/src/ok1.txt" "$WORK/pf/dst/ok1.txt" && cmp -s "$WORK/pf/src/ok2.txt" "$WORK/pf/dst/ok2.txt" \
  && ok "unaffected files still copied" || bad "one bad entry sank the whole transfer"
grep -q 'clash_file' "$WORK/pf1.log" && grep -q 'clash_dir' "$WORK/pf1.log" \
  && ok "errors name the offending paths" || bad "errors do not name the paths"

# The journal must have committed the successes despite the failures.
"$KEVAT" "$WORK/pf/src" "$WORK/pf/dst" >"$WORK/pf2.log" 2>&1
n=$(grep -oE 'resuming: [0-9]+' "$WORK/pf2.log" | grep -oE '[0-9]+' | head -1)
[ "${n:-0}" -ge 2 ] && ok "journal kept the completed files ($n carried forward)" \
  || bad "completed files lost from the journal (carried ${n:-0})"

# Clear the conflicts; the same command must now finish the job.
rm "$WORK/pf/dst/clash_dir"
rm -rf "$WORK/pf/dst/clash_file"
"$KEVAT" "$WORK/pf/src" "$WORK/pf/dst" >"$WORK/pf3.log" 2>&1 \
  && diff -r "$WORK/pf/src" "$WORK/pf/dst" >/dev/null 2>&1 \
  && ok "run completes once the conflicts are cleared" || bad "could not recover after clearing conflicts"

# ── 13. --paranoid must distrust the source too ──────────────────────────────
# It hashed only the destination against the journal's hash — the hash of the *old*
# source. Edit the source in place, keep its size and mtime, and the stale destination
# matched perfectly: exactly the tampering --paranoid claims to catch, skipped.
say "13. --paranoid re-copies a source edited in place"
rm -rf "$WORK/par" "$JOURNALS"
mkdir -p "$WORK/par/src"
head -c 2097152 /dev/urandom > "$WORK/par/src/a.bin"
head -c 300000000 /dev/urandom > "$WORK/par/src/b.bin"
"$KEVAT" "$WORK/par/src" "$WORK/par/dst" >/dev/null 2>&1 &
pid=$!
# a.bin sorts first, so once b.bin's .kpart passes its first 64 MiB checkpoint the
# commit that records it has flushed a.bin's file-done record too.
if kill_at_size "$WORK/par/dst/b.bin.kpart" $((100 * 1024 * 1024)) "$pid"; then
  wait "$pid" 2>/dev/null
  python3 - "$WORK/par/src/a.bin" <<'PY'
import os, sys
p = sys.argv[1]
st = os.stat(p)
data = bytearray(open(p, 'rb').read())
data[0] ^= 0xFF; data[-1] ^= 0xFF          # same length, different bytes
open(p, 'wb').write(data)
# Preserved to the exact nanosecond, so the cheap size+mtime check passes and only
# --paranoid's re-hash can catch the edit — which is the claim under test.
os.utime(p, ns=(st.st_atime_ns, st.st_mtime_ns))
PY
  "$KEVAT" "$WORK/par/src" "$WORK/par/dst" --paranoid >"$WORK/par.log" 2>&1
  grep -qE 'resuming: [1-9]' "$WORK/par.log" \
    && ok "a.bin was journalled as done — the skip path was in play" \
    || bad "a.bin never reached the journal — the test proved nothing"
  cmp -s "$WORK/par/src/a.bin" "$WORK/par/dst/a.bin" \
    && ok "--paranoid re-copied the edited source" || bad "--paranoid trusted a stale destination"
  cmp -s "$WORK/par/src/b.bin" "$WORK/par/dst/b.bin" && ok "b.bin byte-identical" || bad "b.bin differs"
else
  wait "$pid" 2>/dev/null
  printf '  \033[33m~\033[0m copy completed before the checkpoint could be caught — skipped\n'
fi

# ── 14. a mount alias of the source is refused ──────────────────────────────
# Canonicalised path *strings* cannot see mount aliasing: the same volume bind-mounted
# at two places is one inode under two names, and `kevat DIR ALIAS --move` copied every
# file over itself and then unlinked "the source" — deleting everything, exit 0. The
# refusal must come from comparing (st_dev, st_ino), which is what the bind mount
# proves; a symlink alias would be caught by the string comparison alone.
say "14. a mount alias of the source is refused as a destination"
rm -rf "$WORK/alias" "$JOURNALS"
mkdir -p "$WORK/alias/real/sub" "$WORK/alias/mnt"
printf 'irreplaceable\n' > "$WORK/alias/real/keep.txt"
if unshare -rm --propagation private true 2>/dev/null; then
  unshare -rm --propagation private bash -c \
    "mount --bind '$WORK/alias/real' '$WORK/alias/mnt' && exec '$KEVAT' '$WORK/alias/real' '$WORK/alias/mnt' --move" \
    >"$WORK/alias.log" 2>&1
  rc=$?
  grep -q 'same path' "$WORK/alias.log" && [ "$rc" -ne 0 ] \
    && ok "bind-mount alias refused (exit $rc)" \
    || bad "bind-mount alias accepted — a move would delete everything"
  # The aliased spelling of destination-inside-source: no ancestor string matches,
  # but an ancestor inode is the source itself.
  unshare -rm --propagation private bash -c \
    "mount --bind '$WORK/alias/real' '$WORK/alias/mnt' && exec '$KEVAT' '$WORK/alias/real' '$WORK/alias/mnt/sub'" \
    >"$WORK/alias2.log" 2>&1
  rc=$?
  grep -q 'inside the source' "$WORK/alias2.log" && [ "$rc" -ne 0 ] \
    && ok "aliased nested destination refused (exit $rc)" \
    || bad "aliased nested destination accepted"
else
  # No user namespaces available: a symlink alias still exercises the same
  # (dev,ino) comparison, since fs::metadata resolves the link before reading it.
  ln -s "$WORK/alias/real" "$WORK/alias/link"
  "$KEVAT" "$WORK/alias/real" "$WORK/alias/link" --move >"$WORK/alias.log" 2>&1
  rc=$?
  grep -q 'same path' "$WORK/alias.log" && [ "$rc" -ne 0 ] \
    && ok "symlink alias refused (exit $rc)" || bad "symlink alias accepted"
fi
[ -s "$WORK/alias/real/keep.txt" ] && ok "the file survived" || bad "DATA LOST through the alias"

# ── 15. a move must not unlink a source edited within the same second ───────
# The completed-file skip compares mtimes — and that branch unlinks sources in move
# mode. At whole-second precision, an edit that kept the size and landed in the same
# second read as "unchanged": the destination kept the stale bytes and the only copy
# of the edit was deleted, exit 0. The comparison must be at nanosecond precision.
say "15. move re-copies a source edited within the same second"
rm -rf "$WORK/ns" "$JOURNALS"
mkdir -p "$WORK/ns/src"
printf 'original content here\n' > "$WORK/ns/src/f.txt"
chmod 555 "$WORK/ns/src"     # run 1 copies but cannot unlink, so the journal survives
"$KEVAT" "$WORK/ns/src" "$WORK/ns/dst" --move >/dev/null 2>&1
chmod 755 "$WORK/ns/src"
python3 - "$WORK/ns/src/f.txt" <<'PY'
import os, sys
p = sys.argv[1]
st = os.stat(p)
data = bytearray(open(p, 'rb').read())
data[0:8] = b'EDITED!!'                    # same length, different bytes
open(p, 'wb').write(data)
# Pinned into the same whole second as the original, nanoseconds apart — what a real
# save moments after the copy looks like on a fast machine.
os.utime(p, ns=(st.st_atime_ns, st.st_mtime_ns // 1000000000 * 1000000000 + 123456789))
PY
want=$(sha256sum "$WORK/ns/src/f.txt" | cut -d' ' -f1)
"$KEVAT" "$WORK/ns/src" "$WORK/ns/dst" --move >"$WORK/ns.log" 2>&1
rc=$?
got=$(sha256sum "$WORK/ns/dst/f.txt" | cut -d' ' -f1)
[ "$rc" -eq 0 ] && [ "$want" = "$got" ] \
  && ok "destination holds the edited bytes" \
  || bad "stale destination kept while the edited source was unlinked"
[ ! -e "$WORK/ns/src/f.txt" ] && ok "move completed" || bad "source still present after the move"

# ── 16. corruption below the last checkpoint span ────────────────────────────
# Test 5 corrupts inside the *newest* span. While only that span was validated, the
# region below it — growing 64 MiB per checkpoint — was trusted blind: a byte flipped
# at 10 MiB rode through resume into the final output with exit 0. The whole chain
# back to byte zero has to re-hash clean before the offset is trusted.
say "16. corruption below the last checkpoint span is refused"
rm -rf "$WORK/deep" "$JOURNALS"
mkdir -p "$WORK/deep/src"
head -c 300000000 /dev/urandom > "$WORK/deep/src/big.bin"
"$KEVAT" "$WORK/deep/src" "$WORK/deep/dst" >/dev/null 2>&1 &
pid=$!
# Past 200 MiB three checkpoints are durable, so 10 MiB lies two spans below the tip.
if kill_at_size "$WORK/deep/dst/big.bin.kpart" $((200 * 1024 * 1024)) "$pid"; then
  wait "$pid" 2>/dev/null
  python3 -c "
p='$WORK/deep/dst/big.bin.kpart'; off=10*1024*1024
with open(p,'r+b') as f:
    f.seek(off); b=f.read(1); f.seek(off); f.write(bytes([b[0]^0xFF]))"
  "$KEVAT" "$WORK/deep/src" "$WORK/deep/dst" >"$WORK/deep.log" 2>&1
  if grep -q 'resuming big.bin at' "$WORK/deep.log"; then
    bad "trusted a chain with a corrupt early span"
  else
    ok "refused the early corruption"
  fi
  cmp -s "$WORK/deep/src/big.bin" "$WORK/deep/dst/big.bin" \
    && ok "corruption did not reach the output" || bad "corruption reached the output"
else
  wait "$pid" 2>/dev/null
  printf '  \033[33m~\033[0m copy completed before 200 MiB could be caught — skipped\n'
fi

# ── 17. the lock arbitrates the destination, not the journal ─────────────────
# Test 10 covers the same command twice, which shares a journal. The journal is keyed
# on (src, dst, mode), so a second run differing in mode — or in *source*, with a
# colliding relative path — took its own journal lock and wrote into the same
# destination: two processes with the same .kpart open, an output interleaved from
# two sources, both exiting 0.
say "17. concurrent runs against one destination are refused"
rm -rf "$WORK/dlk" "$JOURNALS"
mkdir -p "$WORK/dlk/srcA" "$WORK/dlk/srcB"
head -c 900000000 /dev/urandom > "$WORK/dlk/srcA/big.bin"
head -c 8000000  /dev/urandom > "$WORK/dlk/srcB/big.bin"
"$KEVAT" "$WORK/dlk/srcA" "$WORK/dlk/dst" >/dev/null 2>&1 &
first=$!
if wait_for_size "$WORK/dlk/dst/big.bin.kpart" $((50 * 1024 * 1024)) "$first"; then
  # Same pair, different mode: a distinct journal, the same destination.
  "$KEVAT" "$WORK/dlk/srcA" "$WORK/dlk/dst" --move >"$WORK/dlk1.log" 2>&1
  rc=$?
  grep -q 'already writing to this destination' "$WORK/dlk1.log" && [ "$rc" -ne 0 ] \
    && ok "same pair in --move refused (exit $rc)" \
    || bad "a --move ran concurrently with a copy of the same pair"
  if kill -0 "$first" 2>/dev/null; then
    # Different source, colliding relative path, same destination.
    "$KEVAT" "$WORK/dlk/srcB" "$WORK/dlk/dst" >"$WORK/dlk2.log" 2>&1
    rc=$?
    grep -q 'already writing to this destination' "$WORK/dlk2.log" && [ "$rc" -ne 0 ] \
      && ok "different source into the same destination refused (exit $rc)" \
      || bad "a second source wrote into a destination already in flight"
  else
    printf '  \033[33m~\033[0m first run finished before the second source leg — skipped\n'
  fi
  wait "$first" 2>/dev/null
  cmp -s "$WORK/dlk/srcA/big.bin" "$WORK/dlk/dst/big.bin" \
    && ok "destination intact" || bad "destination corrupted by a concurrent run"
  [ -s "$WORK/dlk/srcA/big.bin" ] && ok "the refused --move unlinked nothing" \
    || bad "the refused --move deleted the source"
else
  wait "$first" 2>/dev/null
  printf '  \033[33m~\033[0m first run finished too quickly to overlap — skipped\n'
fi

# ── 18. a symlink root, and a trailing-slash destination ─────────────────────
# `kevat link.txt out.txt` fell into the directory walk, created a *directory* at the
# destination and exited 0 having copied nothing — `kevat "$f" "$d" && rm "$f"` then
# deleted the data. The root argument must resolve through the link (links inside the
# tree stay skipped), and a run that copied nothing because everything was skipped
# must not exit 0.
say "18. symlink root and trailing-slash destination"
rm -rf "$WORK/ln" "$JOURNALS"
mkdir -p "$WORK/ln/outdir"
printf 'through the link\n' > "$WORK/ln/real.txt"
ln -s "$WORK/ln/real.txt" "$WORK/ln/link.txt"
"$KEVAT" "$WORK/ln/link.txt" "$WORK/ln/out.txt" >/dev/null 2>&1
rc=$?
[ "$rc" -eq 0 ] && [ -f "$WORK/ln/out.txt" ] && cmp -s "$WORK/ln/real.txt" "$WORK/ln/out.txt" \
  && ok "file copied through the link root" || bad "symlink root not copied (exit $rc)"
[ -d "$WORK/ln/out.txt" ] && bad "a directory was created at the destination" || ok "no bogus directory"

mkdir -p "$WORK/ln/onlylinks"
ln -s /nowhere "$WORK/ln/onlylinks/dangling"
"$KEVAT" "$WORK/ln/onlylinks" "$WORK/ln/od" >/dev/null 2>&1
[ $? -ne 0 ] && ok "an all-skipped scan exits non-zero" || bad "copied nothing yet exited 0"

printf 'hello\n' > "$WORK/ln/a.txt"
"$KEVAT" "$WORK/ln/a.txt" "$WORK/ln/outdir/" >/dev/null 2>&1 \
  && cmp -s "$WORK/ln/a.txt" "$WORK/ln/outdir/a.txt" \
  && ok "trailing slash into an existing directory" || bad "trailing slash (existing dir) failed"
"$KEVAT" "$WORK/ln/a.txt" "$WORK/ln/newdir/" >/dev/null 2>&1 \
  && cmp -s "$WORK/ln/a.txt" "$WORK/ln/newdir/a.txt" \
  && ok "trailing slash creates the directory" || bad "trailing slash (new dir) failed"

say "result"
if [ "$fail" -eq 0 ]; then
  printf '  \033[32mall checks passed\033[0m\n'; exit 0
else
  printf '  \033[31m%d check(s) failed\033[0m\n' "$fail"; exit 1
fi
