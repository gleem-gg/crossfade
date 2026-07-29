# COPR SRPM generator. COPR invokes this as:
#   make -f .copr/Makefile srpm outdir="<outdir>" spec="<spec>"
# with the repository clone as the working directory and network access
# available (the actual RPM build later runs offline, hence the vendor
# tarball).
outdir ?= .

.PHONY: srpm
srpm:
	dnf -y install cargo git-core rpm-build tar xz
	set -eux; \
	version=$$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1); \
	release=1.$$(git rev-list --count HEAD).g$$(git rev-parse --short HEAD); \
	workdir=$$(mktemp -d); \
	git archive --format=tar --prefix=gleem-crossfade-$$version/ HEAD \
	    | gzip > "$$workdir/gleem-crossfade-$$version.tar.gz"; \
	cargo vendor --locked "$$workdir/vendor" >/dev/null; \
	tar -C "$$workdir" -cJf "$$workdir/gleem-crossfade-$$version-vendor.tar.xz" vendor; \
	sed -e "s/^Version:.*/Version:        $$version/" \
	    -e "s/^Release:.*/Release:        $$release%{?dist}/" \
	    packaging/gleem-crossfade.spec > "$$workdir/gleem-crossfade.spec"; \
	rpmbuild -bs "$$workdir/gleem-crossfade.spec" \
	    --define "_sourcedir $$workdir" \
	    --define "_srcrpmdir $(outdir)"
