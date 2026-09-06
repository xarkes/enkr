#!/usr/bin/env bash
#
# Run every Enkr test suite.
#
# Lives in the enkr repo but reaches outside it: enkr-proto, enkr-syncd and mae
# are separate checkouts expected as siblings of this one. Suites that need a
# missing sibling are skipped with a note, not failed, so this still does
# something useful in a bare `enkr` clone.
#
# The suites do not all live where you would guess:
#
#   .            the shippable app. `cargo test` here builds ONLY unit tests —
#                `autotests = false` with no `[[test]]` targets is deliberate,
#                because tests/*.rs need the private enkr-syncd relay and the
#                published crate must never resolve it.
#   e2e/         the crate that DOES depend on enkr-syncd. It `#[path]`-includes
#                ../tests/*.rs, so that is where the relay-backed, multi-client
#                suites actually run.
#   ../enkr-proto   protocol + crypto unit tests.
#   ../enkr-syncd   relay unit tests + seq_monotonicity.  (private)
#   ../mae          the UI toolkit, including its own testkit suites.
#
# Usage:
#   ./test.sh              all default suites (unit + e2e + testkit-native)
#   ./test.sh unit         unit tests only, every available crate
#   ./test.sh e2e          relay-backed multi-client suites only
#   ./test.sh testkit      mae testkit: enkr's ::native driver tests + mae's own
#   ./test.sh cdp          browser-backed ::cdp driver tests (SEE "Known broken")
#   ./test.sh -h
#
# Known broken:
#
#   * `cdp` is excluded from the default run and currently fails 12/12. mae
#     commit a793f3b removed the DOM backend (`IMUI::new_dom` / `run_dom`), but
#     src/main.rs and src/bin/test_harness.rs still call it, so the wasm32
#     harness the CdpDriver needs will not compile. The same call sites break
#     the wasm32/web build of the app itself.
#

set -uo pipefail

ENKR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if [[ -t 1 ]]; then
  BOLD=$'\033[1m'; RED=$'\033[31m'; GREEN=$'\033[32m'; YELLOW=$'\033[33m'; DIM=$'\033[2m'; OFF=$'\033[0m'
else
  BOLD=''; RED=''; GREEN=''; YELLOW=''; DIM=''; OFF=''
fi

# Suite results, collected for the closing summary.
declare -a RESULT_NAMES=()
declare -a RESULT_STATES=()
FAILED=0

record() { RESULT_NAMES+=("$1"); RESULT_STATES+=("$2"); }

# run <label> <dir-relative-to-enkr> <cargo args...>
run() {
  local label="$1" dir="$2"; shift 2
  local disp="$dir"; [[ "$disp" == . ]] && disp="enkr"
  printf '\n%s==> %s%s  %s(%s: cargo %s)%s\n' \
    "$BOLD" "$label" "$OFF" "$DIM" "$disp" "$*" "$OFF"
  if (cd "$ENKR/$dir" && cargo "$@"); then
    record "$label" pass
  else
    record "$label" FAIL
    FAILED=1
  fi
}

# A sibling checkout that isn't here is a missing prerequisite, not a failure:
# enkr-syncd is private, and a bare clone of this repo has none of them.
need() {
  local label="$1" dir="$2"
  if [[ -d "$ENKR/$dir" ]]; then
    return 0
  fi
  printf '\n%s==> %s%s  %sskipped: %s not found%s\n' \
    "$BOLD" "$label" "$OFF" "$YELLOW" "$dir" "$OFF"
  record "$label" skip
  return 1
}

suite_unit() {
  run "unit: enkr" . test
  need "unit: enkr-proto" ../enkr-proto && run "unit: enkr-proto" ../enkr-proto test
  need "unit: enkr-syncd" ../enkr-syncd && run "unit: enkr-syncd" ../enkr-syncd test
  need "unit: mae"        ../mae        && run "unit: mae"        ../mae        test
}

# Real sockets, real SQLite files, real clocks:
#   --include-ignored  everything outside sync.rs is #[ignore]d for that reason
#   --test-threads=1   the timeouts are wall-clock, so parallel load causes
#                      spurious failures
suite_e2e() {
  local label="e2e: relay + multi-client"
  need "$label" ../enkr-syncd || return 0
  run "$label" e2e test -- --test-threads=1 --include-ignored
}

# driver_test! generates a ::native and a ::cdp variant per scenario. The native
# ones need no browser and already ride along in `unit: enkr`; running them
# explicitly here is what makes "testkit" mean something on its own.
suite_testkit() {
  run "testkit: enkr ::native" . test --lib -- ::native
  need "testkit: mae" ../mae && run "testkit: mae" ../mae test --features testkit
}

suite_cdp() {
  printf '\n%s%sNote:%s cdp is expected to fail until enkr is ported off mae'\''s\n' \
    "$BOLD" "$YELLOW" "$OFF"
  printf '      removed DOM backend (new_dom/run_dom). See the header of this script.\n'
  run "testkit: enkr ::cdp" . test --lib --features cdp -- ::cdp
}

# The header comment block is the documentation; print it rather than keeping a
# second copy in sync with it.
usage() {
  awk 'NR > 1 { if (/^#/) { sub(/^# ?/, ""); print } else exit }' "${BASH_SOURCE[0]}"
}

case "${1:-all}" in
  all)     suite_unit; suite_e2e; suite_testkit ;;
  unit)    suite_unit ;;
  e2e)     suite_e2e ;;
  testkit) suite_testkit ;;
  cdp)     suite_cdp ;;
  -h|--help|help) usage; exit 0 ;;
  *) printf '%sunknown suite: %s%s\n\n' "$RED" "$1" "$OFF" >&2; usage >&2; exit 2 ;;
esac

printf '\n%s==== summary ====%s\n' "$BOLD" "$OFF"
for i in "${!RESULT_NAMES[@]}"; do
  case "${RESULT_STATES[$i]}" in
    pass) printf '  %sPASS%s  %s\n' "$GREEN" "$OFF" "${RESULT_NAMES[$i]}" ;;
    skip) printf '  %sSKIP%s  %s\n' "$YELLOW" "$OFF" "${RESULT_NAMES[$i]}" ;;
    *)    printf '  %sFAIL%s  %s\n' "$RED"    "$OFF" "${RESULT_NAMES[$i]}" ;;
  esac
done

if [[ $FAILED -ne 0 ]]; then
  printf '\n%sSomething failed.%s The cdp suite is broken outright (see this\n' "$RED" "$OFF"
  printf 'script'\''s header); nothing else here is known to fail, so treat anything\n'
  printf 'else as a real regression rather than re-running it.\n'
  exit 1
fi

printf '\n%sAll green.%s' "$GREEN" "$OFF"
if [[ "${1:-all}" != cdp ]]; then
  printf ' %s(cdp not run — currently broken, see header)%s' "$DIM" "$OFF"
fi
printf '\n'
