#!/bin/bash
#:
#: name = "helios / global zone packages"
#: variety = "basic"
#: target = "helios-2.0"
#: rust_toolchain = "1.72.0"
#: output_rules = [
#:	"=/work/version.txt",
#:	"=/work/switch-asic.tar.gz",
#:	"=/work/global-zone-packages.tar.gz",
#:	"=/work/trampoline-global-zone-packages.tar.gz",
#: ]
#:
#: [[publish]]
#: series = "image"
#: name = "global-zone-packages"
#: from_output = "/work/global-zone-packages.tar.gz"
#:
#: [[publish]]
#: series = "image"
#: name = "trampoline-global-zone-packages"
#: from_output = "/work/trampoline-global-zone-packages.tar.gz"

set -o errexit
set -o pipefail
set -o xtrace

cargo --version
rustc --version

#
# Generate the version for control plane artifacts here. We use `0.git` as the
# prerelease field because it comes before `alpha`.
#
# In this job, we stamp the version into packages installed in the host and
# trampoline global zone images.
#
COMMIT=$(git rev-parse HEAD)
VERSION="1.0.2-0.ci+git${COMMIT:0:11}"
echo "$VERSION" >/work/version.txt

ptime -m ./tools/install_builder_prerequisites.sh -yp

pfexec mkdir -p /work && pfexec chown $USER /work

tarball_src_dir="$(pwd)/out/versioned"
stamp_packages() {
	for package in "$@"; do
		# TODO: remove once https://github.com/oxidecomputer/omicron-package/pull/54 lands
		if [[ $package == maghemite ]]; then
			echo "0.0.0" > VERSION
			tar rvf "out/$package.tar" VERSION
			rm VERSION
		fi

		cargo run --locked --release --bin omicron-package -- stamp "$package" "$VERSION"
	done
}

# Build necessary for the global zone
ptime -m cargo run --locked --release --bin omicron-package -- \
  -t host target create -i standard -m gimlet -s asic
ptime -m cargo run --locked --release --bin omicron-package -- \
  -t host package \
  --only maghemite \
  --only omicron-sled-agent \
  --only overlay \
  --only logadm \
  --only propolis-server
stamp_packages \
  maghemite \
  omicron-sled-agent \
  overlay \
  propolis-server

# Create global zone package @ /work/global-zone-packages.tar.gz
ptime -m ./tools/build-global-zone-packages.sh "$tarball_src_dir" /work

ptime -m cargo run --locked --release --bin omicron-package -- \
  -t host package \
  --only dendrite-asic \
  --only mg-ddm \
  --only omicron-gateway \
  --only omicron-gateway-asic \
  --only omicron-gateway-asic-customizations \
  --only switch_zone_setup \
  --only switch-asic \
  --only wicket \
  --only wicketd \
  --only xcvradm
stamp_packages switch-asic
cp out/switch-asic.tar.gz /work/

#
# Global Zone files for Trampoline image
#

# Build necessary for the trampoline image
ptime -m cargo run --locked --release --bin omicron-package -- \
  -t recovery target create -i trampoline
ptime -m cargo run --locked --release --bin omicron-package -- \
  -t recovery package \
  --only installinator \
  --only maghemite
stamp_packages \
  installinator \
  maghemite

# Create trampoline global zone package @ /work/trampoline-global-zone-packages.tar.gz
ptime -m ./tools/build-trampoline-global-zone-packages.sh "$tarball_src_dir" /work
