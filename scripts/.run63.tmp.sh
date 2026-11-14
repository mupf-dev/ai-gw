set +e
. ~/.cargo/env
export PATH=$HOME/.local/bin:/opt/reuse-venv/bin:$PATH
mkdir -p mess
cargo fmt --all
AIGW_STRICT=1 AIGW_TEST_POSTGRES=docker AIGW_TEST_REDIS=docker bash scripts/check.sh > mess/check.log 2>&1; echo "CHECK=$?"
grep -E 'FEHLER|FAILED|panicked|^error' mess/check.log | head -30
bash scripts/runtime-smoke.sh > mess/smoke.log 2>&1; echo "SMOKE=$?"; tail -5 mess/smoke.log
(cd ui/e2e && AIGW_E2E_PORT=4793 bash run.sh > ../../mess/e2e.log 2>&1); echo "E2E=$?"; grep -E 'passed|failed|flaky|✘' mess/e2e.log | head -20
cargo bench -p aigw-wasm --bench wasm > mess/bench.txt 2>&1; tail -12 mess/bench.txt
nproc > mess/sizes.txt
for v in wasm nowasm; do
  F=""; [ $v = nowasm ] && F="--no-default-features --features mimalloc"
  S=$(date +%s); CARGO_TARGET_DIR=target/m-$v cargo build --release -q -p aigw $F; E=$(date +%s)
  echo "$v build_s=$((E-S)) size=$(stat -c %s target/m-$v/release/aigw)" >> mess/sizes.txt
done
cat mess/sizes.txt
rm -rf target/m-wasm target/m-nowasm
