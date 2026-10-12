#!/usr/bin/env bash
# Rebuild the recognizer from pinned source; never patch a prebuilt binary.
set -euo pipefail
repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
native_root="$(realpath -m -- "${1:-${repo_root}/target/sherpa-native}")"
pins="$(python3 - "${repo_root}" <<'PY'
import hashlib, json, pathlib, sys
root = pathlib.Path(sys.argv[1])
pin = json.loads((root / 'packaging/native/runtime.json').read_text())
actual = hashlib.sha256((root / pin['patch_file']).read_bytes()).hexdigest()
if actual != pin['patch_sha256']:
    raise SystemExit('Native patch does not match its pinned SHA-256')
for key in ['source_revision', 'source_sha256', 'source_url']:
    print(pin[key])
PY
)"
readarray -t fields <<< "${pins}"
revision="${fields[0]}"
source_sha="${fields[1]}"
source_url="${fields[2]}"
mkdir -p "${native_root}"
archive="${native_root}/source.tar.gz"
if [[ ! -f "${archive}" ]]; then
  curl --fail --location --silent --show-error \
    "${source_url}" \
    --output "${archive}"
fi
printf '%s  %s\n' "${source_sha}" "${archive}" | sha256sum --check
source_dir="${native_root}/sherpa-onnx-${revision}"
tar -xzf "${archive}" -C "${native_root}"
patch --batch --fuzz=0 --directory "${source_dir}" -p1 \
  < "${repo_root}/packaging/native/sherpa-initial-context.patch"
cmake -S "${source_dir}" -B "${native_root}/build" -G Ninja \
  -DCMAKE_BUILD_TYPE=Release -DBUILD_SHARED_LIBS=ON \
  -DSHERPA_ONNX_USE_PRE_INSTALLED_ONNXRUNTIME_IF_AVAILABLE=OFF \
  -DSHERPA_ONNX_ENABLE_BINARY=OFF -DSHERPA_ONNX_ENABLE_PORTAUDIO=OFF \
  -DSHERPA_ONNX_ENABLE_WEBSOCKET=OFF -DSHERPA_ONNX_ENABLE_TTS=OFF \
  -DSHERPA_ONNX_ENABLE_SPEAKER_DIARIZATION=OFF \
  -DSHERPA_ONNX_BUILD_C_API_EXAMPLES=OFF \
  -DCMAKE_INSTALL_PREFIX="${native_root}/install"
cmake --build "${native_root}/build" --parallel "${LCRT_NATIVE_BUILD_JOBS:-4}"
cmake --install "${native_root}/build"
# The source pins dependency archive SHA-256 values, including ONNX Runtime.
# Link the state test to this exact source build, with no prebuilt recognizer.
include_flags=("-I${source_dir}")
for directory in kaldi_decoder-src kaldi_native_fbank-src kissfft-src kaldifst-src \
  eigen-src simple-sentencepiece-src json-src/include openfst-src/src/include \
  onnxruntime-src/include; do
  include_flags+=("-I${native_root}/build/_deps/${directory}")
done
g++ -std=c++17 -O2 "${include_flags[@]}" \
  "${repo_root}/experiments/sherpa-ja-vi/native-context-test.cc" \
  -Wl,--start-group "${native_root}"/build/lib/*.a -Wl,--end-group \
  "-L${native_root}/install/lib" -lonnxruntime -pthread \
  '-Wl,-rpath,$ORIGIN/install/lib' \
  -o "${native_root}/native-context-test"
printf 'SHERPA_ONNX_LIB_DIR=%s\n' "${native_root}/install/lib"
