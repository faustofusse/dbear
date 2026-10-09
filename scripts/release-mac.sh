#!/usr/bin/env bash
# Builds a release dbear.app, zips it, writes the Sparkle appcast and (with --publish) uploads both
# as a GitHub release. The app reads releases/latest/download/appcast.xml, so the release is created
# as a draft and only made public once the appcast is attached.
# Usage: scripts/release-mac.sh <version> [--publish]     e.g. scripts/release-mac.sh 0.1.0 --publish
# The tag is v<version> and points at HEAD, so the working tree must be clean to publish.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

VERSION="${1:?usage: scripts/release-mac.sh <version> [--publish]}"
VERSION="${VERSION#v}"
PUBLISH="${2:-}"
TAG="v$VERSION"

if [[ "$PUBLISH" == --publish ]]; then
  [[ -z "$(git status --porcelain)" ]] || { echo "working tree is dirty; commit first" >&2; exit 1; }
  git rev-parse -q --verify "refs/tags/$TAG" >/dev/null && { echo "tag $TAG already exists" >&2; exit 1; }
fi

# Signed with Developer ID and notarized with the notarytool keychain profile NOTARY_PROFILE (default
# "dbear"; create it once with `xcrun notarytool store-credentials dbear --apple-id ... --team-id ...`).
NOTARY_PROFILE="${NOTARY_PROFILE:-dbear}"
IDENTITY="${CODESIGN_IDENTITY:-$(security find-identity -p codesigning -v | sed -n 's/.*"\(Developer ID Application:[^"]*\)".*/\1/p' | head -1)}"
[[ -n "$IDENTITY" ]] || { echo "no Developer ID Application certificate in the keychain" >&2; exit 1; }
xcrun notarytool history --keychain-profile "$NOTARY_PROFILE" >/dev/null || {
  echo "no notarytool profile '$NOTARY_PROFILE'; run: xcrun notarytool store-credentials $NOTARY_PROFILE" >&2; exit 1; }

# The Sparkle private key (login keychain, from generate_keys) must match the public key the app
# embeds, or installed copies would reject the update.
# shellcheck source=scripts/sparkle-tools.sh
source "$ROOT/scripts/sparkle-tools.sh"
EMBEDDED_KEY="$(sed -n 's/^DBEAR_SPARKLE_PUBLIC_KEY="\(.*\)"$/\1/p' "$ROOT/scripts/bundle-mac.sh")"
[[ -n "$EMBEDDED_KEY" ]] || {
  echo "DBEAR_SPARKLE_PUBLIC_KEY is empty in scripts/bundle-mac.sh; run $SPARKLE_BIN/generate_keys" >&2; exit 1; }
if [[ -z "${SPARKLE_SIGN_ARGS:-}" ]]; then
  KEYCHAIN_KEY="$("$SPARKLE_BIN/generate_keys" -p 2>/dev/null)" || {
    echo "no Sparkle private key in the keychain; run $SPARKLE_BIN/generate_keys" >&2; exit 1; }
  [[ "$KEYCHAIN_KEY" == "$EMBEDDED_KEY" ]] || {
    echo "the Sparkle key in the keychain doesn't match DBEAR_SPARKLE_PUBLIC_KEY" >&2; exit 1; }
fi

SPARKLE_PUBLIC_KEY="$EMBEDDED_KEY" VERSION="$VERSION" CODESIGN_IDENTITY="$IDENTITY" DISTRIBUTE=1 \
  "$ROOT/scripts/bundle-mac.sh" release
codesign --verify --deep --strict build/dbear.app

ZIP="build/dbear-$VERSION-macos-arm64.zip"
rm -f "$ZIP"
ditto -c -k --sequesterRsrc --keepParent build/dbear.app "$ZIP"
xcrun notarytool submit "$ZIP" --keychain-profile "$NOTARY_PROFILE" --wait
xcrun stapler staple build/dbear.app
spctl --assess --type execute -vv build/dbear.app
# Re-zip so the download carries the stapled ticket (works offline on first launch).
rm -f "$ZIP"
ditto -c -k --sequesterRsrc --keepParent build/dbear.app "$ZIP"
shasum -a 256 "$ZIP" | tee "$ZIP.sha256"

REPO_URL="https://github.com/faustofusse/dbear"
ZIP_URL="$REPO_URL/releases/download/$TAG/$(basename "$ZIP")"
APPCAST=build/appcast.xml
if [[ "$PUBLISH" != --publish ]]; then
  "$ROOT/scripts/write-appcast.sh" "$ZIP" "$VERSION" "$ZIP_URL" "$APPCAST" "" "$REPO_URL/releases/tag/$TAG" >/dev/null
  echo "built $ZIP and $APPCAST (pass --publish to upload)"
  exit 0
fi

git tag -a "$TAG" -m "dbear $VERSION"
git push origin "$TAG"
NOTES_TEXT="$(cat <<'EOF'
**macOS** (Apple silicon, macOS 15 or later): download the `macos-arm64` zip, unzip it and move
`dbear.app` to `/Applications`.

**Windows** (10 or later): run `dbear-…-windows-x64-setup.exe` (installs for your user, no
administrator needed), or unzip the portable `windows-x64` zip. The release workflow adds the
Windows files a few minutes after the release appears.

Later versions update themselves.
EOF
)"
if gh release view "$TAG" >/dev/null 2>&1; then
  # .github/workflows/release-windows.yml creates a draft when the Mac release doesn't show up.
  gh release upload "$TAG" "$ZIP" "$ZIP.sha256" --clobber
  gh release edit "$TAG" --title "dbear $VERSION" --notes "$NOTES_TEXT"
else
  gh release create "$TAG" "$ZIP" "$ZIP.sha256" --draft --title "dbear $VERSION" --generate-notes --notes "$NOTES_TEXT"
fi
# The appcast carries the release notes (markdown, shown by Check for Updates…).
NOTES=build/release-notes.md
gh release view "$TAG" --json body --jq .body >"$NOTES"
"$ROOT/scripts/write-appcast.sh" "$ZIP" "$VERSION" "$ZIP_URL" "$APPCAST" "$NOTES" "$REPO_URL/releases/tag/$TAG" >/dev/null
gh release upload "$TAG" "$APPCAST"
gh release edit "$TAG" --draft=false --latest
