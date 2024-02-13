include /usr/share/dpkg/pkg-info.mk

SRCPACKAGE=rust-proxmox-fuse
PACKAGE=lib$(SRCPACKAGE)-dev
ARCH:=$(shell dpkg-architecture -qDEB_BUILD_ARCH)

DEB=$(PACKAGE)_$(DEB_VERSION)_$(ARCH).deb
DSC=$(SRCPACKAGE)_$(DEB_VERSION)_$(ARCH).deb

.PHONY: all
all: check

.PHONY: check
check:
	cargo test --all-features

.PHONY: dinstall
dinstall: deb
	sudo -k dpkg -i build/librust-*.deb

build:
	rm -rf build
	rm debian/control
	mkdir build
	debcargo package \
	    --config "$(PWD)/debian/debcargo.toml" \
	    --changelog-ready \
	    --no-overlay-write-back \
	    --directory "$(PWD)/build/proxmox-fuse" \
	    "proxmox-fuse" \
	    "$$(dpkg-parsechangelog -l "debian/changelog" -SVersion | sed -e 's/-.*//')"
	echo system >build/rust-toolchain
	rm -f build/proxmox-fuse/Cargo.lock
	find build/proxmox-fuse/debian -name '*.hint' -delete
	cp build/proxmox-fuse/debian/control debian/control

.PHONY: deb
deb:
	rm -rf build
	$(MAKE) build/$(DEB)
build/$(DEB): build
	(cd build/proxmox-fuse && CARGO=/usr/bin/cargo RUSTC=/usr/bin/rustc dpkg-buildpackage -b -uc -us)
	lintian build/*.deb

.PHONY: dsc
dsc:
	rm -rf build
	$(MAKE) build/$(DSC)
build/$(DSC): build
	(cd build/proxmox-fuse && CARGO=/usr/bin/cargo RUSTC=/usr/bin/rustc dpkg-buildpackage -S -uc -us)
	lintian build/*.dsc

.PHONY: clean
clean:
	rm -rf build
	cargo clean

.PHONY: upload
upload: UPLOAD_DIST ?= $(DEB_DISTRIBUTION)
upload: build/$(DEB)
	cd build; \
	    dcmd --deb rust-proxmox-fuse_*.changes \
	    | grep -v '.changes$$' \
	    | tar -cf- -T- \
	    | ssh -X repoman@repo.proxmox.com upload --product devel --dist $(UPLOAD_DIST)
