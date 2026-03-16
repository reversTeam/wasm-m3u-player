#!/usr/bin/env bash
set -euo pipefail

HOOK_DIR="$(git rev-parse --git-dir)/hooks"
mkdir -p "$HOOK_DIR"

cat > "$HOOK_DIR/pre-push" << 'HOOK'
#!/usr/bin/env bash
set -euo pipefail

echo "🔍 Running pre-push checks..."

# Format check
echo "  → cargo fmt --check"
if ! cargo fmt --all -- --check 2>/dev/null; then
    echo "❌ Formatting issues found. Run 'cargo fmt --all' to fix."
    exit 1
fi

# Clippy (native crates)
echo "  → clippy (native)"
if ! cargo clippy --workspace --exclude player-wasm -- -D warnings 2>/dev/null; then
    echo "❌ Clippy errors in native crates."
    exit 1
fi

# Clippy (WASM) — best-effort, skip if wasm32 std lib not available
if cargo check --target wasm32-unknown-unknown -p player-wasm 2>/dev/null; then
    echo "  → clippy (WASM)"
    if ! cargo clippy --target wasm32-unknown-unknown -p player-wasm -- -D warnings 2>/dev/null; then
        echo "❌ Clippy errors in player-wasm (WASM target)."
        exit 1
    fi
else
    echo "  ⚠ Skipping WASM clippy (wasm32-unknown-unknown toolchain not functional)"
fi

echo "✅ All pre-push checks passed!"
HOOK

chmod +x "$HOOK_DIR/pre-push"
echo "✅ pre-push hook installed at $HOOK_DIR/pre-push"
