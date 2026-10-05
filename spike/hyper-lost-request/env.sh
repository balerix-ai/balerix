# Source me: the spike's build and run environment (balerix#129).
W=/workspace/.claude/worktrees/hyper-spike
export CARGO_TARGET_DIR=$W/target/spike
export TMPDIR=$W/target/spike-tmp
export RUSTFLAGS="--cfg hyper_unstable_tracing"
export ENVTEST_DIR=$HOME/.local/share/mise/installs/github-kubernetes-sigs-controller-tools/envtest-v1.34.1/envtest
mkdir -p "$TMPDIR"
