#!/bin/bash
#
# Tested on Ubuntu 18.04
#

#we need clang when compiling on ARMv7
#export CC=/usr/bin/clang
#export CXX=/usr/bin/clang++

TOPDIR=$(pwd)

echo "Init git submodules ..."
git submodule update --init --recursive

cd 3rdparty/ravenna-alsa-lkm/driver
# The submodule's own default remote (see ../../../.gitmodules) is upstream bondagit/ravenna-alsa-lkm --
# left as-is so it stays easy to track upstream. The driver actually built here comes from our own
# fork instead, which carries changes upstream doesn't have yet (currently: real per-leg PTP status
# for SMPTE 2022-7 redundancy, BCP-008 TX stream status bits, and a raised ALSA channel ceiling).
git remote get-url fork >/dev/null 2>&1 || git remote add fork https://github.com/GBA-TAB/ravenna-alsa-lkm.git
git fetch fork
git checkout -B experimental-hw-timestamping fork/experimental-hw-timestamping
make
cd -

echo "Building webui (Blazor WebAssembly) ..."
# `dotnet publish -o` lands the real static site one level deeper, at <out>/wwwroot/ (alongside
# non-web files like web.config) - not directly at <out>/ - so copy that up into webui/dist, which
# is what daemon.conf's http_base_dir already points at.
cd webui-blazor
rm -rf publish
dotnet publish -c Release -o publish
rm -rf ../webui/dist
mkdir -p ../webui/dist
cp -r publish/wwwroot/. ../webui/dist/
rm -rf publish
cd ..

cd daemon

echo "Building aes67-daemon ..."
cmake \
	-DBoost_NO_WARN_NEW_VERSIONS=1 \
	-DCPP_HTTPLIB_DIR="${TOPDIR}/3rdparty/cpp-httplib" \
	-DRAVENNA_ALSA_LKM_DIR="${TOPDIR}/3rdparty/ravenna-alsa-lkm" \
	-DENABLE_TESTS=ON \
	-DWITH_AVAHI=ON \
	-DFAKE_DRIVER=OFF \
	-DWITH_SYSTEMD=ON \
	-DWITH_STREAMER=ON \
	-DWITH_NMOS=ON \
	.
make
cd ..
cd test
make
cd ..

