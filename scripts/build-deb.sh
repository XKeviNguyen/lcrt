#!/usr/bin/env bash
# Builds the LILOPOP Debian package for the host architecture from a release build.
#
# Usage: scripts/build-deb.sh [output-directory]
#
# The package includes the models listed in packaging/models.json. They are
# fetched into LCRT_MODEL_CACHE (default: target/share/lcrt/models) and used
# only if their SHA-256 matches the manifest; a mismatch fails the build.
#
# Runtime dependencies are derived from the linked libraries with
# dpkg-shlibdeps, so build on the Ubuntu release you are packaging for.
# File timestamps come from the last commit, so identical sources and
# toolchains produce identical packages.
set -euo pipefail

readonly APP_ID="io.github.hoangnguyen7474.Lcrt"
readonly PACKAGE="lcrt"

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
output_dir="$(realpath -m -- "${1:-${repo_root}/target/debian}")"
cd -- "${repo_root}"

for tool in cargo dpkg-deb dpkg-shlibdeps python3 rustc strip; do
  command -v "${tool}" >/dev/null || {
    printf 'missing required tool: %s\n' "${tool}" >&2
    exit 1
  }
done

version="$(cargo metadata --locked --no-deps --format-version 1 |
  python3 -c 'import json, sys
print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"] == "lcrt-app"))')"
architecture="$(dpkg --print-architecture)"
export SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct)}"

cargo build --locked --release -p lcrt-app
model_cache="$(realpath -m -- "${LCRT_MODEL_CACHE:-${repo_root}/target/share/lcrt/models}")"
python3 scripts/fetch-models.py "${model_cache}"
python3 scripts/prepare-translation.py
python_abi="$(python3 -c 'import sys; print(f"{sys.version_info.major}.{sys.version_info.minor}")')"

work_dir="$(mktemp -d)"
trap 'rm -rf -- "${work_dir}"' EXIT
stage="${work_dir}/${PACKAGE}"
doc_dir="${stage}/usr/share/doc/${PACKAGE}"
umask 022

install -Dm755 target/release/lcrt "${stage}/usr/bin/lcrt"
strip --strip-unneeded "${stage}/usr/bin/lcrt"
install -Dm644 "packaging/linux/${APP_ID}.desktop" \
  "${stage}/usr/share/applications/${APP_ID}.desktop"
install -Dm644 "packaging/linux/${APP_ID}.metainfo.xml" \
  "${stage}/usr/share/metainfo/${APP_ID}.metainfo.xml"
install -Dm644 "packaging/linux/${APP_ID}.svg" \
  "${stage}/usr/share/icons/hicolor/scalable/apps/${APP_ID}.svg"
install -Dm644 "packaging/linux/${APP_ID}.png" \
  "${stage}/usr/share/icons/hicolor/256x256/apps/${APP_ID}.png"
# Models, each at its manifest destination: file, destination, license file.
python3 - "${model_cache}" "${stage}" <<'MODELS'
import json, os, shutil, sys
cache, stage = sys.argv[1:3]
for model in json.load(open('packaging/models.json'))['models']:
    target = os.path.join(stage, model['destination'])
    os.makedirs(os.path.dirname(target), exist_ok=True)
    shutil.copyfile(os.path.join(cache, model['file']), target)
    os.chmod(target, 0o644)
MODELS
cp -a target/share/lcrt/translation "${stage}/usr/share/lcrt/translation"
find "${stage}/usr/share/lcrt/translation" -type d -name __pycache__ -prune -exec rm -rf {} +
install -Dm644 packaging/licenses/opus-CC-BY-4.0.txt "${doc_dir}/opus-CC-BY-4.0.txt"
install -Dm644 packaging/translation-models.json "${doc_dir}/translation-models.json"
install -Dm644 packaging/translation-runtime.json "${doc_dir}/translation-runtime.json"
install -Dm644 README.md "${doc_dir}/README.md"
install -Dm644 docs/PRIVACY.md "${doc_dir}/PRIVACY.md"

# Debian copyright: LILOPOP itself plus every Rust crate linked into the binary
# (including the vendored whisper.cpp) with its declared license.
{
  printf 'Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/\n'
  printf 'Upstream-Name: LILOPOP\nSource: https://github.com/hoangnguyen7474/lcrt\n'
  printf 'Comment: Statically linked third-party crates and their licenses:\n'
  cargo tree --locked -e normal -p lcrt-app --prefix none --format ' {p}: {l}' \
    --target "$(rustc -vV | sed -n 's/^host: //p')" |
    grep -v ' (/' | sed 's/ (\*)$//; s/ v\([0-9]\)/ \1/' | LC_ALL=C sort -u
  printf '\nFiles: *\nCopyright: %s\nLicense: MIT\n' \
    "$(sed -n 's/^Copyright (c) //p' LICENSE)"
  sed '1,/^Copyright (c)/d; s/^$/./; s/^/ /' LICENSE
  # Each bundled model with its source, pinned revision and license text.
  python3 - <<'MODELS'
import json
for model in json.load(open('packaging/models.json'))['models']:
    print(f"\nFiles: {model['destination']}")
    print(f"Copyright: {model['copyright']}")
    print(f"License: {model['license']}")
    print(f"Comment: {model['purpose']}.")
    print(f" Source: {model['url']}")
    print(f" SHA-256: {model['sha256']}")
    for line in open(model['license_file']).read().rstrip().splitlines():
        print(' ' + (line or '.'))
for model in json.load(open('packaging/translation-models.json'))['models']:
    print(f"\nFiles: usr/share/lcrt/translation/{model['pair']}/*")
    print(f"Copyright: {model['copyright']}")
    print(f"License: {model['license']}")
    print(f"Comment: {model['url']}; SHA-256: {model['sha256']}")
    for line in open('packaging/licenses/opus-CC-BY-4.0.txt').read().rstrip().splitlines():
        print(' ' + (line or '.'))
print("\nComment: Runtime wheel pins are recorded in translation-runtime.json.")
print(" Runtime copyrights and licenses are included under usr/share/lcrt/translation/runtime.")
MODELS
} >"${doc_dir}/copyright"
chmod 644 "${doc_dir}/copyright"

# dpkg-shlibdeps reads package names from a debian/control file.
mkdir -p "${work_dir}/shlibs/debian"
printf 'Source: %s\n\nPackage: %s\nArchitecture: any\n' "${PACKAGE}" "${PACKAGE}" \
  >"${work_dir}/shlibs/debian/control"
depends="$(cd "${work_dir}/shlibs" &&
  dpkg-shlibdeps -O "${stage}/usr/bin/lcrt" 2>/dev/null |
  sed -n 's/^shlibs:Depends=//p')"
test -n "${depends}"

installed_size="$(du -sk --apparent-size "${stage}" | cut -f1)"
mkdir -p "${stage}/DEBIAN"
cat >"${stage}/DEBIAN/control" <<CONTROL
Package: ${PACKAGE}
Version: ${version}
Architecture: ${architecture}
Maintainer: LILOPOP contributors <lcrt@users.noreply.github.com>
Installed-Size: ${installed_size}
Depends: ${depends}, python${python_abi}
Recommends: pipewire, gnome-keyring
Section: sound
Priority: optional
Homepage: https://github.com/hoangnguyen7474/lcrt
Description: LILOPOP live captions and offline translation
 LILOPOP shows real-time captions for system audio or a microphone.
 Offline Captions work right after installation with the included
 multilingual speech model (Whisper Tiny); audio stays on the device.
 Online Captions, Online Translation and vocabulary explanations use
 the user's own OpenAI API key, stored in the desktop keyring, and stream
 audio or selected text to OpenAI only while in use. API charges may apply.
 Offline Translation runs locally for Japanese↔English and Vietnamese↔English.
 LILOPOP has no telemetry.
CONTROL

find "${stage}" -exec touch --no-dereference --date="@${SOURCE_DATE_EPOCH}" {} +
mkdir -p "${output_dir}"
deb="${output_dir}/${PACKAGE}_${version}_${architecture}.deb"
dpkg-deb --root-owner-group -Zxz --build "${stage}" "${deb}" >/dev/null
printf '%s\n' "${deb}"
