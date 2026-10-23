{
  description = "microscope Linux debugger development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    rust-overlay.url = "github:oxalica/rust-overlay";
  };

  outputs = { self, nixpkgs, rust-overlay }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs {
        inherit system;
        overlays = [ rust-overlay.overlays.default ];
      };
      rust = pkgs.rust-bin.nightly."2026-07-11".default.override {
        extensions = [ "clippy" "miri" "rust-src" "rustfmt" ];
        # For fully static builds of microscope: `just build-static`.
        targets = [ "x86_64-unknown-linux-musl" ];
      };
      goStable = pkgs.go.overrideAttrs (_final: previous: {
        version = "1.27.1";
        src = pkgs.fetchurl {
          url = "https://go.dev/dl/go1.27.1.src.tar.gz";
          hash = "sha256-TkCKuuEm2Ra2FkYnGT8sVPDjyhMS1pO4bbRfhiqyOLE=";
        };
        # The patch that relaxes module vendoring for nixpkgs' Go builders no
        # longer applies to 1.27, and nothing here builds with them.
        patches = builtins.filter
          (patch: !(pkgs.lib.hasInfix "go_no_vendor_checks" (toString patch)))
          previous.patches;
      });

      # Builds C++ against LLVM's libc++ instead of libstdc++, so fixtures
      # cover both standard libraries' layouts.
      libcxx = pkgs.llvmPackages.libcxx;
      clangLibcxx = pkgs.writeShellScriptBin "clang++-libc++" ''
        exec ${pkgs.clang}/bin/clang++ -stdlib=libc++ -nostdinc++ \
          -isystem ${libcxx.dev}/include/c++/v1 \
          -L${libcxx}/lib -Wl,-rpath,${libcxx}/lib "$@"
      '';

      # GCC and Clang targeting musl instead of glibc, so fixtures cover both
      # C libraries' thread layouts. Wrapping them keeps the cross toolchain's
      # environment out of the shell's native one.
      musl64 = pkgs.pkgsCross.musl64;
      # Static musl builds of the libraries a static microscope links: musl and
      # LLVM's libunwind, which Rust's standard library unwinds with.
      pkgsStatic = pkgs.pkgsStatic;
      muslGcc = pkgs.writeShellScriptBin "musl-gcc" ''
        exec ${musl64.stdenv.cc}/bin/x86_64-unknown-linux-musl-gcc "$@"
      '';
      muslClang = pkgs.writeShellScriptBin "musl-clang" ''
        exec ${musl64.buildPackages.clang}/bin/x86_64-unknown-linux-musl-clang "$@"
      '';
      # Ubuntu's builds of glibc: the loader and libraries the TLS fixtures
      # built for them run on, in lib, and the startup files they link with,
      # in dev. 18.04's and 20.04's describe their thread structures in
      # libpthread; 22.04's in libc, as every glibc since 2.34 does.
      ubuntuPackage = name: version: hash: pkgs.fetchurl {
        urls = [
          "https://launchpad.net/ubuntu/+archive/primary/+files/${name}_${version}_amd64.deb"
          "http://archive.ubuntu.com/ubuntu/pool/main/g/glibc/${name}_${version}_amd64.deb"
        ];
        inherit hash;
      };
      ubuntuGlibc = { version, package, runtime, development }: pkgs.runCommand "ubuntu-glibc-${version}" {
        runtime = ubuntuPackage "libc6" package runtime;
        development = ubuntuPackage "libc6-dev" package development;
        nativeBuildInputs = [ pkgs.binutils pkgs.zstd ];
      } ''
        unpack() { mkdir "$2"; (cd "$2" && ar x "$1" && tar xf data.tar.*); }
        unpack "$runtime" runtime
        unpack "$development" development
        mkdir -p "$out/dev"
        mv runtime/lib/x86_64-linux-gnu "$out/lib"
        cd development/usr/lib/x86_64-linux-gnu
        for file in Scrt1.o crti.o crtn.o libc_nonshared.a libpthread_nonshared.a; do
          if [ -e "$file" ]; then cp "$file" "$out/dev"; fi
        done
      '';
      ubuntuGlibcs = pkgs.linkFarm "ubuntu-glibc" (map
        (release: { name = release.version; path = ubuntuGlibc release; })
        [
          {
            version = "2.27";
            package = "2.27-3ubuntu1.6";
            runtime = "sha256-W4E9RRJJHNj7MLLrkZI0nudf/jyfIa4TbMeyQp9ww7I=";
            development = "sha256-lq59LPOPBUfKPMyzNEyIAv31vNccS22a7QlVisjMh9s=";
          }
          {
            version = "2.31";
            package = "2.31-0ubuntu9.18";
            runtime = "sha256-GyKBqsSTXf6h+J38GeRF/btFMDICr2BnnMuL8DXwgaA=";
            development = "sha256-EQKZAPIxW5POSHeSrhvxXwkn5eUIgMCR5qRAsU1OFko=";
          }
          {
            version = "2.35";
            package = "2.35-0ubuntu3.15";
            runtime = "sha256-eeNSViJ+FqYHwVTN6412/xLSDjHeKG6n/ZrTuW/gRS0=";
            development = "sha256-zlGuIzqfgAoEDeLQzVmD74b1WMYjs0LICUIwHX9FP0g=";
          }
        ]);
      # CPython interpreters, one directory each, so that Python support is
      # tested against the builds people run. Each directory holds `python`, the
      # interpreter; `include`, the directory with its Python.h; and, for
      # nixpkgs' builds, `debug`, the interpreter's separate DWARF. They are
      # reached only through MICROSCOPE_CPYTHON, never through the shell's
      # packages, so no interpreter's libraries reach the shell's environment.
      cpythonNixpkgs = version: package: pkgs.linkFarm "cpython-${version}" [
        { name = "python"; path = "${package}/bin/python${version}"; }
        { name = "include"; path = "${package}/include/python${version}"; }
        # The debug output splits the DWARF out of the interpreter, as
        # distributions do, so debuggers read its symbols from a lib/debug tree.
        { name = "debug"; path = "${package.debug}"; }
      ];
      # The same, with `site`, a directory holding numpy built for that
      # interpreter, without DWARF, as numpy's wheels are. A fixture that
      # draws arrays puts it on its path; the free-threaded builds have none,
      # since nixpkgs builds their numpy from source.
      cpythonNumpy = version: package: pkgs.linkFarm "cpython-${version}" [
        { name = "python"; path = "${package}/bin/python${version}"; }
        { name = "include"; path = "${package}/include/python${version}"; }
        { name = "debug"; path = "${package.debug}"; }
        { name = "site"; path = "${package.pkgs.numpy}/${package.sitePackages}"; }
      ];
      # Rewrites the file offset of the dynamic string table's section header to
      # the one its segment maps the table from. python-build-standalone's
      # BOLT-optimized interpreters keep a copy of the table at the end of the
      # file and name that copy in the section header, so patchelf moves the
      # wrong bytes and the interpreter's symbols fail to resolve.
      fixDynstr = pkgs.writeText "fix-dynstr.py" ''
        import struct
        import sys

        path = sys.argv[1]
        with open(path, "r+b") as f:
            data = bytearray(f.read())
            phoff, = struct.unpack_from("<Q", data, 0x20)
            shoff, = struct.unpack_from("<Q", data, 0x28)
            phentsize, phnum = struct.unpack_from("<HH", data, 0x36)
            shentsize, shnum, shstrndx = struct.unpack_from("<HHH", data, 0x3A)
            loads = []
            for i in range(phnum):
                p = phoff + i * phentsize
                ptype, = struct.unpack_from("<I", data, p)
                if ptype == 1:  # PT_LOAD
                    offset, vaddr, _, filesz = struct.unpack_from("<QQQQ", data, p + 8)
                    loads.append((offset, vaddr, filesz))
            names, = struct.unpack_from("<Q", data, shoff + shstrndx * shentsize + 24)
            for i in range(shnum):
                s = shoff + i * shentsize
                start = names + struct.unpack_from("<I", data, s)[0]
                name = bytes(data[start:data.index(b"\0", start)])
                if name != b".dynstr":
                    continue
                addr, = struct.unpack_from("<Q", data, s + 16)
                for offset, vaddr, filesz in loads:
                    if vaddr <= addr < vaddr + filesz:
                        struct.pack_into("<Q", data, s + 24, offset + addr - vaddr)
            f.seek(0)
            f.write(data)
      '';
      # python-build-standalone's relocatable builds, which the Python ecosystem
      # ships. Their interpreter names /lib64/ld-linux-x86-64.so.2, which the
      # pure shell lacks, so that one ELF header is patched to the flake's glibc.
      # Nothing else changes: nixpkgs' fixup would rewrite the RUNPATHs the
      # build set, so the output skips it.
      cpythonStandalone = { binary, url, hash }: pkgs.runCommand "cpython-${binary}" {
        src = pkgs.fetchurl { inherit url hash; };
        nativeBuildInputs = [ pkgs.gnutar pkgs.gzip pkgs.patchelf ];
        dontStrip = true;
        dontPatchELF = true;
        dontFixup = true;
      } ''
        mkdir -p $out/install
        tar xzf $src -C $out/install --strip-components=1
        real=$(readlink -f $out/install/bin/${binary})
        ${pkgs.python3}/bin/python3 ${fixDynstr} "$real"
        patchelf --set-interpreter ${pkgs.glibc}/lib/ld-linux-x86-64.so.2 "$real"
        ln -s install/bin/${binary} $out/python
        ln -s install/include/${binary} $out/include
      '';
      # A distribution's own CPython, built against that distribution's C
      # library and libraries. Its .deb packages are unpacked into one root, as
      # ubuntuGlibc unpacks glibc, keeping the distribution's /usr layout, so
      # the interpreter finds its standard library from its own path. The
      # interpreter and its extension modules name the root's loader and
      # libraries, since the shell's loader would otherwise search its own, and
      # only those ELF headers change.
      cpythonDistribution = { version, debs }: pkgs.runCommand "cpython-distribution-${version}" {
        debs = map pkgs.fetchurl debs;
        nativeBuildInputs = [ pkgs.binutils pkgs.gnutar pkgs.xz pkgs.zstd pkgs.patchelf ];
        dontStrip = true;
        dontPatchELF = true;
        dontFixup = true;
      } ''
        mkdir -p $out/root
        for deb in $debs; do
          unpack=$(mktemp -d)
          (cd "$unpack" && ar x "$deb" && tar xf data.tar.*)
          cp -a "$unpack"/. $out/root/
          rm -rf "$unpack"
        done
        libdirs=$out/root/usr/lib/x86_64-linux-gnu
        patchelf --set-interpreter $out/root/usr/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2 --set-rpath $libdirs $out/root/usr/bin/python${version}
        # A library's own dependencies are searched only in its RUNPATH, so each
        # library in the root names the root's directory, not the host's.
        find $out/root/usr/lib/x86_64-linux-gnu -maxdepth 1 -name '*.so*' -type f ! -name 'ld-linux*' -exec patchelf --set-rpath $libdirs {} \;
        for so in $out/root/usr/lib/python${version}/lib-dynload/*.so; do
          patchelf --set-rpath $libdirs "$so"
        done
        ln -s root/usr/bin/python${version} $out/python
        ln -s root/usr/include/python${version} $out/include
        # Python.h includes pyconfig.h through its multiarch path, so the
        # compiler needs the root's include directory itself, not just the
        # python directory.
        mkdir $out/include-arch
        ln -s ../root/usr/include/x86_64-linux-gnu $out/include-arch/x86_64-linux-gnu
      '';
      debianDebs = [
        {
          hash = "sha256-wf2p57G/FbL6daI42KhxDhQv4UxyL8ZQxfxhTweMkHM=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20260906T023042Z/pool/main/p/python3.13/python3.13-minimal_3.13.5-2+deb13u5_amd64.deb"
            "https://deb.debian.org/debian/pool/main/p/python3.13/python3.13-minimal_3.13.5-2+deb13u5_amd64.deb"
          ];
        }
        {
          hash = "sha256-pKpqmox3+Hv+VF4dFOxCwAHRiazpDjqnXe7IgrOd7kk=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20260906T023042Z/pool/main/p/python3.13/libpython3.13-minimal_3.13.5-2+deb13u5_amd64.deb"
            "https://deb.debian.org/debian/pool/main/p/python3.13/libpython3.13-minimal_3.13.5-2+deb13u5_amd64.deb"
          ];
        }
        {
          hash = "sha256-2xYTIqNIHSwMO5o7mgPDqw4rFxj1S4h1X7Sj+TkWW4Q=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20260906T023042Z/pool/main/p/python3.13/libpython3.13-stdlib_3.13.5-2+deb13u5_amd64.deb"
            "https://deb.debian.org/debian/pool/main/p/python3.13/libpython3.13-stdlib_3.13.5-2+deb13u5_amd64.deb"
          ];
        }
        {
          hash = "sha256-S9exf/vrNnzh/vi2/wJTaiXyn8BzctG90+0eH6pblAk=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20260906T023042Z/pool/main/p/python3.13/libpython3.13-dev_3.13.5-2+deb13u5_amd64.deb"
            "https://deb.debian.org/debian/pool/main/p/python3.13/libpython3.13-dev_3.13.5-2+deb13u5_amd64.deb"
          ];
        }
        {
          hash = "sha256-lnqmJgVyEIHD6yoXZQYRp5KqgC12plEdGEAkJiPSBMk=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20260711T202405Z/pool/main/g/glibc/libc6_2.41-12+deb13u4_amd64.deb"
            "https://deb.debian.org/debian/pool/main/g/glibc/libc6_2.41-12+deb13u4_amd64.deb"
          ];
        }
        {
          hash = "sha256-AVvnQNYjatEUWC3qUAwdkH8p4W1tsAVmyjL7aNcayQ0=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20241029T234230Z/pool/main/z/zlib/zlib1g_1.3.dfsg+really1.3.1-1+b1_amd64.deb"
            "https://deb.debian.org/debian/pool/main/z/zlib/zlib1g_1.3.dfsg+really1.3.1-1+b1_amd64.deb"
          ];
        }
        {
          hash = "sha256-/v+HR3LK6QSuTSTpF7NWZJUr7EtK6RAKTt2lp/p0aDI=";
          urls = [
            "https://snapshot.debian.org/archive/debian-security/20261016T061025Z/pool/updates/main/e/expat/libexpat1_2.9.0-1~deb13u1_amd64.deb"
            "https://security.debian.org/debian-security/pool/updates/main/e/expat/libexpat1_2.9.0-1~deb13u1_amd64.deb"
          ];
        }
        {
          hash = "sha256-Dr3DQN4zMzY5w8Y4dM1LFawug9+h7zBTt+769JGfT2g=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20250425T084555Z/pool/main/libf/libffi/libffi8_3.4.8-2_amd64.deb"
            "https://deb.debian.org/debian/pool/main/libf/libffi/libffi8_3.4.8-2_amd64.deb"
          ];
        }
        {
          hash = "sha256-y6TNoEJEteSBuxVSS8PJg6fRtvMwATubOBcGovy2UxA=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20240819T204354Z/pool/main/b/bzip2/libbz2-1.0_1.0.8-6_amd64.deb"
            "https://deb.debian.org/debian/pool/main/b/bzip2/libbz2-1.0_1.0.8-6_amd64.deb"
          ];
        }
        {
          hash = "sha256-s4rKsw8pW/EGamYy4xy9ChmA8pJLOAQM2hMdXepThOw=";
          urls = [
            "https://snapshot.debian.org/archive/debian-security/20261007T211543Z/pool/updates/main/x/xz-utils/liblzma5_5.8.1-1+deb13u2_amd64.deb"
            "https://security.debian.org/debian-security/pool/updates/main/x/xz-utils/liblzma5_5.8.1-1+deb13u2_amd64.deb"
          ];
        }
        {
          hash = "sha256-/xa8BIvNfRslYJRFC3nHeUfY52/iokvZm5ECHVkfoHQ=";
          urls = [
            "https://snapshot.debian.org/archive/debian-security/20260930T060347Z/pool/updates/main/o/openssl/libssl3t64_3.5.7-1~deb13u3_amd64.deb"
            "https://security.debian.org/debian-security/pool/updates/main/o/openssl/libssl3t64_3.5.7-1~deb13u3_amd64.deb"
          ];
        }
        {
          hash = "sha256-CkWa2v/ZARCfeBGrZfWOepV7SQfQVTnPPRGE79zeBGg=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20260714T142842Z/pool/main/s/sqlite3/libsqlite3-0_3.46.1-7+deb13u2_amd64.deb"
            "https://deb.debian.org/debian/pool/main/s/sqlite3/libsqlite3-0_3.46.1-7+deb13u2_amd64.deb"
          ];
        }
        {
          hash = "sha256-wb9MTD/0jFf6v5MwfftWmWtgz6M5J6/EFYtds2+ych4=";
          urls = [
            "https://snapshot.debian.org/archive/debian-security/20260814T165831Z/pool/updates/main/u/util-linux/libuuid1_2.41.5-0+deb13u1_amd64.deb"
            "https://security.debian.org/debian-security/pool/updates/main/u/util-linux/libuuid1_2.41.5-0+deb13u1_amd64.deb"
          ];
        }
        {
          hash = "sha256-R7qi4RV5WDZUWDoGa2m9Wl0vIkIwgfcH7WwMO6bVOLc=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20250306T211336Z/pool/main/n/ncurses/libncursesw6_6.5+20250216-2_amd64.deb"
            "https://deb.debian.org/debian/pool/main/n/ncurses/libncursesw6_6.5+20250216-2_amd64.deb"
          ];
        }
        {
          hash = "sha256-i59qeYPpQYVk5IpidRjeTAORe1bv5o1/PpO9j/+hzBA=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20250306T211336Z/pool/main/n/ncurses/libtinfo6_6.5+20250216-2_amd64.deb"
            "https://deb.debian.org/debian/pool/main/n/ncurses/libtinfo6_6.5+20250216-2_amd64.deb"
          ];
        }
        {
          hash = "sha256-7q3ytedVyfGDiD/uotm1soVgKEJ11fVNPlXQkjsdCWc=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20241213T203501Z/pool/main/r/readline/libreadline8t64_8.2-6_amd64.deb"
            "https://deb.debian.org/debian/pool/main/r/readline/libreadline8t64_8.2-6_amd64.deb"
          ];
        }
        {
          hash = "sha256-DrwUTWYuMZeYLRvzp7izXKhF5UxogR3gMosfDXxnWFw=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20250117T031715Z/pool/main/libx/libxcrypt/libcrypt1_4.4.38-1_amd64.deb"
            "https://deb.debian.org/debian/pool/main/libx/libxcrypt/libcrypt1_4.4.38-1_amd64.deb"
          ];
        }
        {
          hash = "sha256-PHGRe0kNGheu1DGWonh6JW7PBgUmzbICFqdL7cBhsVA=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20250315T204837Z/pool/main/g/gcc-14/libgcc-s1_14.2.0-19_amd64.deb"
            "https://deb.debian.org/debian/pool/main/g/gcc-14/libgcc-s1_14.2.0-19_amd64.deb"
          ];
        }
      ];
      ubuntuDebs = [
        {
          hash = "sha256-1FJom5ZghFNFpMPgXkrYLAgtVHTgQDG3qkfxptVhCm4=";
          urls = [
            "https://launchpad.net/ubuntu/+archive/primary/+files/python3.12-minimal_3.12.3-1ubuntu0.17_amd64.deb"
            "https://archive.ubuntu.com/ubuntu/pool/main/p/python3.12/python3.12-minimal_3.12.3-1ubuntu0.17_amd64.deb"
          ];
        }
        {
          hash = "sha256-1katcRK1rewhug4a8BXwSujKf179xhliJUfdZnPEwUs=";
          urls = [
            "https://launchpad.net/ubuntu/+archive/primary/+files/libpython3.12-minimal_3.12.3-1ubuntu0.17_amd64.deb"
            "https://archive.ubuntu.com/ubuntu/pool/main/p/python3.12/libpython3.12-minimal_3.12.3-1ubuntu0.17_amd64.deb"
          ];
        }
        {
          hash = "sha256-RdP1MLofnW6HmtRrkgRvq6sT/lCoJFDk9lVXo7utFIk=";
          urls = [
            "https://launchpad.net/ubuntu/+archive/primary/+files/libpython3.12-stdlib_3.12.3-1ubuntu0.17_amd64.deb"
            "https://archive.ubuntu.com/ubuntu/pool/main/p/python3.12/libpython3.12-stdlib_3.12.3-1ubuntu0.17_amd64.deb"
          ];
        }
        {
          hash = "sha256-wSPdq3dj5FGZ4htKyFRecF8goDFbmtpMI2tS+wiMThY=";
          urls = [
            "https://launchpad.net/ubuntu/+archive/primary/+files/libpython3.12-dev_3.12.3-1ubuntu0.17_amd64.deb"
            "https://archive.ubuntu.com/ubuntu/pool/main/p/python3.12/libpython3.12-dev_3.12.3-1ubuntu0.17_amd64.deb"
          ];
        }
        {
          hash = "sha256-/1VX2ZtR92HEt8kjaLnMRVZe2hffm/nrSxNNCYJQCL4=";
          urls = [
            "https://launchpad.net/ubuntu/+archive/primary/+files/libc6_2.39-0ubuntu8.9_amd64.deb"
            "https://archive.ubuntu.com/ubuntu/pool/main/g/glibc/libc6_2.39-0ubuntu8.9_amd64.deb"
          ];
        }
        {
          hash = "sha256-hLnPV1KynJ+SwnzUxLqbvMcLXM+bG1FUIaKK4jIS4nM=";
          urls = [
            "https://launchpad.net/ubuntu/+archive/primary/+files/zlib1g_1.3.dfsg-3.1ubuntu2.2_amd64.deb"
            "https://archive.ubuntu.com/ubuntu/pool/main/z/zlib/zlib1g_1.3.dfsg-3.1ubuntu2.2_amd64.deb"
          ];
        }
        {
          hash = "sha256-SUuOZy9yITDGvKanvEzDGkPKiRox1g2Gi/3WmaPCCxM=";
          urls = [
            "https://launchpad.net/ubuntu/+archive/primary/+files/libexpat1_2.6.1-2ubuntu0.6_amd64.deb"
            "https://archive.ubuntu.com/ubuntu/pool/main/e/expat/libexpat1_2.6.1-2ubuntu0.6_amd64.deb"
          ];
        }
        {
          hash = "sha256-Y35qd0TeCM0zGkH079DSTm6pBkhD3qnRxsqHvbXwOKI=";
          urls = [
            "https://launchpad.net/ubuntu/+archive/primary/+files/libffi8_3.4.6-1build1_amd64.deb"
            "https://archive.ubuntu.com/ubuntu/pool/main/libf/libffi/libffi8_3.4.6-1build1_amd64.deb"
          ];
        }
        {
          hash = "sha256-Nu4IsAse5QGLL84CiBt9LXPJtOMpkL3jOIQA2GxIEmo=";
          urls = [
            "https://launchpad.net/ubuntu/+archive/primary/+files/libbz2-1.0_1.0.8-5.1ubuntu0.1_amd64.deb"
            "https://archive.ubuntu.com/ubuntu/pool/main/b/bzip2/libbz2-1.0_1.0.8-5.1ubuntu0.1_amd64.deb"
          ];
        }
        {
          hash = "sha256-0uq9Qcp30sLdnV1O9HjMy2T/3mJ5xHz0aZqFfUZ4WlI=";
          urls = [
            "https://launchpad.net/ubuntu/+archive/primary/+files/liblzma5_5.6.1+really5.4.5-1ubuntu0.3_amd64.deb"
            "https://archive.ubuntu.com/ubuntu/pool/main/x/xz-utils/liblzma5_5.6.1+really5.4.5-1ubuntu0.3_amd64.deb"
          ];
        }
        {
          hash = "sha256-IZ9Dsc2Dak2lUJONtf2pMWDSadOaTtzx8c5pikcNt5c=";
          urls = [
            "https://launchpad.net/ubuntu/+archive/primary/+files/libssl3t64_3.0.13-0ubuntu3.16_amd64.deb"
            "https://archive.ubuntu.com/ubuntu/pool/main/o/openssl/libssl3t64_3.0.13-0ubuntu3.16_amd64.deb"
          ];
        }
        {
          hash = "sha256-sRkLtyNZ9fzEdAaqRgZer08coggIXFEiSlKwS+3AtLs=";
          urls = [
            "https://launchpad.net/ubuntu/+archive/primary/+files/libsqlite3-0_3.45.1-1ubuntu2.8_amd64.deb"
            "https://archive.ubuntu.com/ubuntu/pool/main/s/sqlite3/libsqlite3-0_3.45.1-1ubuntu2.8_amd64.deb"
          ];
        }
        {
          hash = "sha256-fimS06efwFUU3FlmVY7pxmpyFKR+3yo2AXueJ3dShmU=";
          urls = [
            "https://launchpad.net/ubuntu/+archive/primary/+files/libuuid1_2.39.3-9ubuntu6.6_amd64.deb"
            "https://archive.ubuntu.com/ubuntu/pool/main/u/util-linux/libuuid1_2.39.3-9ubuntu6.6_amd64.deb"
          ];
        }
        {
          hash = "sha256-V5yUiHVdHd1w9mSAyv0vaHdvaDFNFdnPS1RJ0N4PyWg=";
          urls = [
            "https://launchpad.net/ubuntu/+archive/primary/+files/libncursesw6_6.4+20240113-1ubuntu2.2_amd64.deb"
            "https://archive.ubuntu.com/ubuntu/pool/main/n/ncurses/libncursesw6_6.4+20240113-1ubuntu2.2_amd64.deb"
          ];
        }
        {
          hash = "sha256-tnpt8r2rYdJz6r69IIu2o/sSYYo5CgZ+HuMFpAn/A9k=";
          urls = [
            "https://launchpad.net/ubuntu/+archive/primary/+files/libtinfo6_6.4+20240113-1ubuntu2.2_amd64.deb"
            "https://archive.ubuntu.com/ubuntu/pool/main/n/ncurses/libtinfo6_6.4+20240113-1ubuntu2.2_amd64.deb"
          ];
        }
        {
          hash = "sha256-Vjl3oW3wO2EfUjnMHpoEJuhkefzGFrXJ4gDqMgYxGeU=";
          urls = [
            "https://launchpad.net/ubuntu/+archive/primary/+files/libreadline8t64_8.2-4build1_amd64.deb"
            "https://archive.ubuntu.com/ubuntu/pool/main/r/readline/libreadline8t64_8.2-4build1_amd64.deb"
          ];
        }
        {
          hash = "sha256-lHR4XNbzmFEr+MMFw5AduxEVadzLb1gyACNzwKisWDI=";
          urls = [
            "https://launchpad.net/ubuntu/+archive/primary/+files/libcrypt1_4.4.36-4build1_amd64.deb"
            "https://archive.ubuntu.com/ubuntu/pool/main/libx/libxcrypt/libcrypt1_4.4.36-4build1_amd64.deb"
          ];
        }
        {
          hash = "sha256-qn+tvjO3i8+ZiFMYBAYBxVDCCJKVZbF5iR2aPMKqaM0=";
          urls = [
            "https://launchpad.net/ubuntu/+archive/primary/+files/libgcc-s1_14.2.0-4ubuntu2~24.04.1_amd64.deb"
            "https://archive.ubuntu.com/ubuntu/pool/main/g/gcc-14/libgcc-s1_14.2.0-4ubuntu2~24.04.1_amd64.deb"
          ];
        }
      ];
      cpython = pkgs.linkFarm "cpython" [
        { name = "3.12"; path = cpythonNumpy "3.12" pkgs.python312; }
        { name = "3.13"; path = cpythonNumpy "3.13" pkgs.python313; }
        { name = "3.13t"; path = cpythonNixpkgs "3.13t" pkgs.python313FreeThreading; }
        { name = "3.14"; path = cpythonNumpy "3.14" pkgs.python314; }
        { name = "3.14t"; path = cpythonNixpkgs "3.14t" pkgs.python314FreeThreading; }
        {
          name = "3.15";
          path = cpythonStandalone {
            binary = "python3.15";
            url = "https://github.com/astral-sh/python-build-standalone/releases/download/20261013/cpython-3.15.0%2B20261013-x86_64-unknown-linux-gnu-install_only_stripped.tar.gz";
            hash = "sha256-aXQNzZs0gLPVN2Dy07dVgq2xT6VENGKN6jJIDd7eVCo=";
          };
        }
        {
          name = "3.15t";
          path = cpythonStandalone {
            binary = "python3.15t";
            url = "https://github.com/astral-sh/python-build-standalone/releases/download/20261013/cpython-3.15.0%2B20261013-x86_64-unknown-linux-gnu-freethreaded-install_only_stripped.tar.gz";
            hash = "sha256-r9AgpIDOF2fWrBtW6XtZxsBvheOpWkL2muFG0loHaJ4=";
          };
        }
        # What uv installs for 3.14: linked into its executable, built by clang
        # with LTO and PGO, and evaluating by tail calls, as 3.15 above does.
        {
          name = "standalone-3.14";
          path = cpythonStandalone {
            binary = "python3.14";
            url = "https://github.com/astral-sh/python-build-standalone/releases/download/20261013/cpython-3.14.8%2B20261013-x86_64-unknown-linux-gnu-install_only_stripped.tar.gz";
            hash = "sha256-2PqhtYhQa54+Wj9XgHuwVtU0oWQaaxDRW26j8LQTuWA=";
          };
        }
        { name = "debian-3.13"; path = cpythonDistribution { version = "3.13"; debs = debianDebs; }; }
        { name = "ubuntu-3.12"; path = cpythonDistribution { version = "3.12"; debs = ubuntuDebs; }; }
      ];

      # Node.js builds, one directory each, so that Node support is tested
      # against the builds people run. Each directory holds `node`, the
      # executable, and `include`, Node's headers for Node-API fixtures;
      # nixpkgs' builds also hold `bin`, with npm and npx beside node. They are
      # reached only through MICROSCOPE_NODE, never through the shell's
      # packages, whose node serves the web tooling.
      nodeNixpkgs = line: package: pkgs.linkFarm "node-nixpkgs-${line}" [
        { name = "node"; path = "${package}/bin/node"; }
        { name = "include"; path = "${package}/include/node"; }
        { name = "bin"; path = "${package}/bin"; }
      ];
      # nodejs.org's release binaries, what nvm, Docker images, and CI's setup
      # actions install. Their interpreter names /lib64/ld-linux-x86-64.so.2,
      # which the pure shell lacks, so the executable names the flake's glibc
      # and finds libstdc++, libgcc_s, and Node 26's libatomic through its
      # rpath. Nothing else changes, and every symbol stays in place.
      nodeOfficial = { version, hash }: pkgs.runCommand "node-official-${version}" {
        src = pkgs.fetchurl {
          url = "https://nodejs.org/dist/v${version}/node-v${version}-linux-x64.tar.xz";
          inherit hash;
        };
        nativeBuildInputs = [ pkgs.gnutar pkgs.xz pkgs.patchelf ];
        dontStrip = true;
        dontPatchELF = true;
        dontFixup = true;
      } ''
        mkdir -p $out/release
        tar xJf $src -C $out/release --strip-components=1 node-v${version}-linux-x64/bin/node node-v${version}-linux-x64/include
        patchelf --set-interpreter ${pkgs.glibc}/lib/ld-linux-x86-64.so.2 \
          --set-rpath ${pkgs.glibc}/lib:${pkgs.stdenv.cc.cc.lib}/lib $out/release/bin/node
        ln -s release/bin/node $out/node
        ln -s release/include/node $out/include
      '';
      # Debian's own Node, which links V8 from a shared libnode.so.137, run
      # unmodified: its packages are unpacked into one root, nothing patched,
      # since it needs a newer glibc than the flake's. It still names
      # /lib64/ld-linux-x86-64.so.2 and reads Debian's JavaScript from
      # /usr/share/nodejs, so it runs only under `enter`, which starts a
      # command in a rootless user and mount namespace with Debian's loader
      # bound at that path, a cache naming the root's libraries bound at
      # /etc/ld.so.cache, the root's /usr/share bound in place, and a /tmp of
      # its own: the host's root is no user in there, so microscope would not
      # trust the host's /tmp with its agent files. `node` is then Debian's
      # executable itself, never a wrapper or the loader, so a debugger
      # launching it traces that executable. The flake's programs keep running
      # inside: nixpkgs' loader reads neither path, and microscope and its
      # tests share the one namespace. The packages are the closure
      # scripts/debian-node-debs.sh prints.
      nodeDebian = { version, debs }: pkgs.runCommand "node-debian-${version}" {
        debs = map pkgs.fetchurl debs;
        nativeBuildInputs = [ pkgs.binutils pkgs.gnutar pkgs.xz pkgs.zstd ];
        dontStrip = true;
        dontPatchELF = true;
        dontFixup = true;
      } ''
        mkdir -p $out/root
        for deb in $debs; do
          unpack=$(mktemp -d)
          (cd "$unpack" && ar x "$deb" && tar xf data.tar.*)
          rm "$unpack"/data.tar.* "$unpack"/control.tar.* "$unpack"/debian-binary
          cp -a "$unpack"/. $out/root/
          rm -rf "$unpack"
        done
        # Debian's loader finds libraries through the cache before its own
        # directories, which the host need not have, so the cache names the
        # root's files by their store paths. ldconfig always adds its own
        # glibc's directory, which would list the flake's libc beside
        # Debian's, unless it runs chrooted, here in a copy of the directory
        # at the same path.
        lib=$out/root/usr/lib/x86_64-linux-gnu
        chroot=$(mktemp -d)
        mkdir -p $chroot/etc $chroot$(dirname $lib)
        touch $chroot/etc/ld.so.conf
        cp -a $lib $chroot$lib
        ${pkgs.glibc.bin}/bin/ldconfig -X -r $chroot -C /ld.so.cache $lib
        mv $chroot/ld.so.cache $out/ld.so.cache
        rm -rf $chroot
        ln -s root/usr/bin/node $out/node
        ln -s root/usr/include/node $out/include
        substitute ${pkgs.writeShellScript "enter" ''
          set -euo pipefail
          for target in /lib64/ld-linux-x86-64.so.2 /etc/ld.so.cache /usr/share; do
            if [[ ! -e "$target" ]]; then
              echo "error: $target is missing, so Debian's node has nowhere to be bound" >&2
              exit 1
            fi
          done
          exec ${pkgs.util-linux}/bin/unshare -rm ${pkgs.bash}/bin/bash -euo pipefail -c '
            ${pkgs.util-linux}/bin/mount --bind "$1/root/usr/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2" /lib64/ld-linux-x86-64.so.2
            ${pkgs.util-linux}/bin/mount --bind "$1/ld.so.cache" /etc/ld.so.cache
            ${pkgs.util-linux}/bin/mount --bind "$1/root/usr/share" /usr/share
            ${pkgs.util-linux}/bin/mount -t tmpfs -o mode=1777 tmpfs /tmp
            shift
            exec "$@"
          ' enter "@out@" "$@"
        ''} $out/enter --subst-var out
        chmod +x $out/enter
      '';
      # nodejs, libnode137, and libnode-dev 24.22.0 from sid's snapshot of
      # 20261022T203737Z, with everything they depend on:
      # `scripts/debian-node-debs.sh 20261022T203737Z` prints the list.
      debianNodeDebs = [
        {
          hash = "sha256-TALvw5m782oLUQJOfRAkpSYPXtyPhVOj7TObzEPTsrM=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20261022T203737Z/pool/main/a/acorn/node-acorn_8.17.0+ds+~cs28.27.25-1_all.deb"
            "https://deb.debian.org/debian/pool/main/a/acorn/node-acorn_8.17.0+ds+~cs28.27.25-1_all.deb"
          ];
        }
        {
          hash = "sha256-xTdqUhSn3AXnBHp34ydtYjrrjMSV/HsSVaAKGdkTY+k=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20261022T203737Z/pool/main/a/ada-url/libada-url0-3_3.4.4-1+b1_amd64.deb"
            "https://deb.debian.org/debian/pool/main/a/ada-url/libada-url0-3_3.4.4-1+b1_amd64.deb"
          ];
        }
        {
          hash = "sha256-BEmGxL9wtilx+IhoXxFoMSOjJv6wKVv1VeU0bt5/hho=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20261022T203737Z/pool/main/b/brotli/libbrotli1_1.2.0-4+b1_amd64.deb"
            "https://deb.debian.org/debian/pool/main/b/brotli/libbrotli1_1.2.0-4+b1_amd64.deb"
          ];
        }
        {
          hash = "sha256-wMEvHpML0R8xF9S0Bv5GLa2t6nq/MLp3sYPd9qVm4xo=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20261022T203737Z/pool/main/c/c-ares/libcares2_1.34.8-1_amd64.deb"
            "https://deb.debian.org/debian/pool/main/c/c-ares/libcares2_1.34.8-1_amd64.deb"
          ];
        }
        {
          hash = "sha256-fcxkBLRK2KnVjoPB6Yfuy01qJGI6kkOO6xGCntESljU=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20261022T203737Z/pool/main/g/gcc-16/gcc-16-base_16.2.0-3_amd64.deb"
            "https://deb.debian.org/debian/pool/main/g/gcc-16/gcc-16-base_16.2.0-3_amd64.deb"
          ];
        }
        {
          hash = "sha256-5xbci6rSfohMRf+Dp2lVBHp6njOoDpT5RZ7pluu4ALA=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20261022T203737Z/pool/main/g/gcc-16/libgcc-s1_16.2.0-3_amd64.deb"
            "https://deb.debian.org/debian/pool/main/g/gcc-16/libgcc-s1_16.2.0-3_amd64.deb"
          ];
        }
        {
          hash = "sha256-fJ4aiebAuSAmmZV7RpwG0LhvXJfobB/5ErZQLd2Hk7Y=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20261022T203737Z/pool/main/g/gcc-16/libstdc++6_16.2.0-3_amd64.deb"
            "https://deb.debian.org/debian/pool/main/g/gcc-16/libstdc++6_16.2.0-3_amd64.deb"
          ];
        }
        {
          hash = "sha256-XHFxXFEQO+tPu6n+n43/YEr2YQ36cccL/zQvVKeGCAM=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20261022T203737Z/pool/main/g/glibc/libc6_2.43-7_amd64.deb"
            "https://deb.debian.org/debian/pool/main/g/glibc/libc6_2.43-7_amd64.deb"
          ];
        }
        {
          hash = "sha256-irrkC14r/KWPvIODKbSPUlyYRj6TC40+0HXQyelgpXo=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20261022T203737Z/pool/main/g/glibc/libc-gconv-modules-extra_2.43-7_amd64.deb"
            "https://deb.debian.org/debian/pool/main/g/glibc/libc-gconv-modules-extra_2.43-7_amd64.deb"
          ];
        }
        {
          hash = "sha256-3JGOpsvnu2/upDqsOSCoaincxfr0xW2PCUhAj5B22GY=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20261022T203737Z/pool/main/i/icu/libicu78_78.3-2_amd64.deb"
            "https://deb.debian.org/debian/pool/main/i/icu/libicu78_78.3-2_amd64.deb"
          ];
        }
        {
          hash = "sha256-e4VIBWBRa03+QdKWK0u6nEpi3nepD/MRx7kIfPElMjk=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20261022T203737Z/pool/main/libh/libhdr-histogram/libhdr-histogram6_0.11.8-4+b2_amd64.deb"
            "https://deb.debian.org/debian/pool/main/libh/libhdr-histogram/libhdr-histogram6_0.11.8-4+b2_amd64.deb"
          ];
        }
        {
          hash = "sha256-1G3p+klPA+WsaXTjTO4HFZ1gCULlDQNqGXYxWO0G6MU=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20261022T203737Z/pool/main/libu/libuv1/libuv1-dev_1.53.0-1_amd64.deb"
            "https://deb.debian.org/debian/pool/main/libu/libuv1/libuv1-dev_1.53.0-1_amd64.deb"
          ];
        }
        {
          hash = "sha256-NkaqW4UT3Fes4k3+ZYK6VmbhQeuHa3SU6QCHdsnUGZk=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20261022T203737Z/pool/main/libu/libuv1/libuv1t64_1.53.0-1_amd64.deb"
            "https://deb.debian.org/debian/pool/main/libu/libuv1/libuv1t64_1.53.0-1_amd64.deb"
          ];
        }
        {
          hash = "sha256-k+eTDkwluRjx3ZgMwcbUSHZUokj1+nHqy5du17vpVL0=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20261022T203737Z/pool/main/libz/libzstd/libzstd1_1.5.7+dfsg-4_amd64.deb"
            "https://deb.debian.org/debian/pool/main/libz/libzstd/libzstd1_1.5.7+dfsg-4_amd64.deb"
          ];
        }
        {
          hash = "sha256-BRn0mPp71M62DVQGZ53G5E80jQwKysi811o0pbUVVog=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20261022T203737Z/pool/main/l/llhttp/libllhttp9.4_9.4.3+~cs12.11.9-2_amd64.deb"
            "https://deb.debian.org/debian/pool/main/l/llhttp/libllhttp9.4_9.4.3+~cs12.11.9-2_amd64.deb"
          ];
        }
        {
          hash = "sha256-GeUg5NRbANtOM/TkhxqzEZkvk6ccTU2LeFlKcYZyjg4=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20261022T203737Z/pool/main/l/llhttp/node-llhttp_9.4.3+~cs12.11.9-2_all.deb"
            "https://deb.debian.org/debian/pool/main/l/llhttp/node-llhttp_9.4.3+~cs12.11.9-2_all.deb"
          ];
        }
        {
          hash = "sha256-z3awqqca+v0bPNnk0j3eE/xDbWt+amodldj8bHNe+tI=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20261022T203737Z/pool/main/n/nghttp2/libnghttp2-14_1.70.0-1_amd64.deb"
            "https://deb.debian.org/debian/pool/main/n/nghttp2/libnghttp2-14_1.70.0-1_amd64.deb"
          ];
        }
        {
          hash = "sha256-mKHEZr/2uvZ4DEt2XsZl5oRCS/eQdDmTm1+xFg7eA5w=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20261022T203737Z/pool/main/n/node-balanced-match/node-balanced-match_2.0.0-1_all.deb"
            "https://deb.debian.org/debian/pool/main/n/node-balanced-match/node-balanced-match_2.0.0-1_all.deb"
          ];
        }
        {
          hash = "sha256-cP8TIuE0GNKkaey96ZjSd6XsufQs5QcStebnEu19Yzw=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20261022T203737Z/pool/main/n/node-brace-expansion/node-brace-expansion_2.1.7+~1.1.2-1_all.deb"
            "https://deb.debian.org/debian/pool/main/n/node-brace-expansion/node-brace-expansion_2.1.7+~1.1.2-1_all.deb"
          ];
        }
        {
          hash = "sha256-NDioz39olotc/MD6WoUHNqc83MTn8Y5V8mIf0rkJYaU=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20261022T203737Z/pool/main/n/node-corepack/node-corepack_0.35.0-2_all.deb"
            "https://deb.debian.org/debian/pool/main/n/node-corepack/node-corepack_0.35.0-2_all.deb"
          ];
        }
        {
          hash = "sha256-r7+HAE9VjGCojkQi4KbYfiKeBtfkVhSwCg45EIKvD1A=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20261022T203737Z/pool/main/n/node-debug/node-debug_4.4.3+~4.1.13-1_all.deb"
            "https://deb.debian.org/debian/pool/main/n/node-debug/node-debug_4.4.3+~4.1.13-1_all.deb"
          ];
        }
        {
          hash = "sha256-6v7P8Xu1PHUB+iNG4keOnE/9KS5mUfJpGXLkZDx+B78=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20261022T203737Z/pool/main/n/nodejs/libnode137_24.22.0+dfsg+~cs24.19.2-2_amd64.deb"
            "https://deb.debian.org/debian/pool/main/n/nodejs/libnode137_24.22.0+dfsg+~cs24.19.2-2_amd64.deb"
          ];
        }
        {
          hash = "sha256-e/LK9IS4iu3M1K6NtsWEFtLjWs/eiYcVGdJNOAalD8g=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20261022T203737Z/pool/main/n/nodejs/libnode-dev_24.22.0+dfsg+~cs24.19.2-2_amd64.deb"
            "https://deb.debian.org/debian/pool/main/n/nodejs/libnode-dev_24.22.0+dfsg+~cs24.19.2-2_amd64.deb"
          ];
        }
        {
          hash = "sha256-IGZztI0JWGqX0WrQWYR0snGaWqufZJN8dSGJkLlNGW4=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20261022T203737Z/pool/main/n/nodejs/nodejs_24.22.0+dfsg+~cs24.19.2-2_amd64.deb"
            "https://deb.debian.org/debian/pool/main/n/nodejs/nodejs_24.22.0+dfsg+~cs24.19.2-2_amd64.deb"
          ];
        }
        {
          hash = "sha256-lMmbXCDdMT75F+dKnBAKao2szVsmHYunh+QXEwobCvE=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20261022T203737Z/pool/main/n/node-lru-cache/node-lru-cache_11.2.7-7_all.deb"
            "https://deb.debian.org/debian/pool/main/n/node-lru-cache/node-lru-cache_11.2.7-7_all.deb"
          ];
        }
        {
          hash = "sha256-dwEmEC4k2Obwn8+9ZlZbA6dwN5GDN7IS4Tc2e8pTHGo=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20261022T203737Z/pool/main/n/node-minimatch/node-minimatch_9.0.7-2_all.deb"
            "https://deb.debian.org/debian/pool/main/n/node-minimatch/node-minimatch_9.0.7-2_all.deb"
          ];
        }
        {
          hash = "sha256-ccmp+LU8wVRbiHNdxPSH8t6ytaQjy9lRROoY0cMYfec=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20261022T203737Z/pool/main/n/node-ms/node-ms_2.1.3+~cs0.7.31-3_all.deb"
            "https://deb.debian.org/debian/pool/main/n/node-ms/node-ms_2.1.3+~cs0.7.31-3_all.deb"
          ];
        }
        {
          hash = "sha256-SpeORpddSNE+ZviCo77t0GTXUHPSSaojAk2CwwOkd4E=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20261022T203737Z/pool/main/n/node-semver/node-semver_7.7.4+~cs9.7.4-2_all.deb"
            "https://deb.debian.org/debian/pool/main/n/node-semver/node-semver_7.7.4+~cs9.7.4-2_all.deb"
          ];
        }
        {
          hash = "sha256-fV9sTDUh2o3lJAaG+O6apctUT31cXLWpgWByXcXIRi4=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20261022T203737Z/pool/main/n/node-undici/node-undici_8.12.2+dfsg+~cs3.2.3-2_all.deb"
            "https://deb.debian.org/debian/pool/main/n/node-undici/node-undici_8.12.2+dfsg+~cs3.2.3-2_all.deb"
          ];
        }
        {
          hash = "sha256-xk43++pA3gErWa2/ph5wO1LBJxPzTL/S1ASevMVJ8nA=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20261022T203737Z/pool/main/n/node-xtend/node-xtend_4.0.2-3_all.deb"
            "https://deb.debian.org/debian/pool/main/n/node-xtend/node-xtend_4.0.2-3_all.deb"
          ];
        }
        {
          hash = "sha256-Cg7IT+hgxck8dNU8uwca6geTMno3YbSDCA6de3PCGzc=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20261022T203737Z/pool/main/o/openssl/libssl3t64_3.6.5-1_amd64.deb"
            "https://deb.debian.org/debian/pool/main/o/openssl/libssl3t64_3.6.5-1_amd64.deb"
          ];
        }
        {
          hash = "sha256-g8VnsG+hNEB16A9+JDIKxS2RA5huq5CNOICehMewAjY=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20261022T203737Z/pool/main/o/openssl/libssl-dev_3.6.5-1_amd64.deb"
            "https://deb.debian.org/debian/pool/main/o/openssl/libssl-dev_3.6.5-1_amd64.deb"
          ];
        }
        {
          hash = "sha256-grfAVvff2R5O7bGgc41lWGe24TvSXOA0BunPRk6TRa0=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20261022T203737Z/pool/main/o/openssl/openssl-provider-legacy_3.6.5-1_amd64.deb"
            "https://deb.debian.org/debian/pool/main/o/openssl/openssl-provider-legacy_3.6.5-1_amd64.deb"
          ];
        }
        {
          hash = "sha256-a/6rvWHKfNM89MReTj0vzHMWO9Q4fUkuvMo3tjOsqTg=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20261022T203737Z/pool/main/s/simdjson/libsimdjson33_4.6.11-1_amd64.deb"
            "https://deb.debian.org/debian/pool/main/s/simdjson/libsimdjson33_4.6.11-1_amd64.deb"
          ];
        }
        {
          hash = "sha256-vdKWSJWe2eniMcLw1Son9RGKaWYUnV1A8CwGrGiR2O0=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20261022T203737Z/pool/main/s/simdutf/libsimdutf35_9.3.0+really9.1.2-1_amd64.deb"
            "https://deb.debian.org/debian/pool/main/s/simdutf/libsimdutf35_9.3.0+really9.1.2-1_amd64.deb"
          ];
        }
        {
          hash = "sha256-u/E8kyZ2TQXjfmWQ3r2qbzOva9gi48oSBHidnR3COxE=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20261022T203737Z/pool/main/s/sqlite3/libsqlite3-0_3.53.4-2_amd64.deb"
            "https://deb.debian.org/debian/pool/main/s/sqlite3/libsqlite3-0_3.53.4-2_amd64.deb"
          ];
        }
        {
          hash = "sha256-UsWFsHvqcu82353dXRk39HOdPK7AV9gnlUuuwlYpJlE=";
          urls = [
            "https://snapshot.debian.org/archive/debian/20261022T203737Z/pool/main/z/zlib/zlib1g_1.3.dfsg+really1.3.2-3_amd64.deb"
            "https://deb.debian.org/debian/pool/main/z/zlib/zlib1g_1.3.dfsg+really1.3.2-3_amd64.deb"
          ];
        }
      ];
      nodes = pkgs.linkFarm "node" [
        { name = "26"; path = nodeOfficial { version = "26.12.1"; hash = "sha256-YXCy1HThxib0PP863BGN+f47VzJYpPw9hQyNvGsHm70="; }; }
        { name = "24"; path = nodeOfficial { version = "24.22.1"; hash = "sha256-bBcqmrZxj3uAH7WnvUhq8u5eMnWAhqBLhPzOKqdwleY="; }; }
        # Node 22 leaves at its end of life, on 30 April 2027.
        { name = "22"; path = nodeOfficial { version = "22.23.4"; hash = "sha256-GAW8015oFVWb0uuX20ONP9sOs6XmqXe3wbrcp2M36c4="; }; }
        { name = "nixpkgs-26"; path = nodeNixpkgs "26" pkgs.nodejs_26; }
        { name = "nixpkgs-24"; path = nodeNixpkgs "24" pkgs.nodejs_24; }
        { name = "nixpkgs-22"; path = nodeNixpkgs "22" pkgs.nodejs_22; }
        { name = "debian-24"; path = nodeDebian { version = "24"; debs = debianNodeDebs; }; }
      ];

      # V8's Torque output for each supported V8 version, which completes the
      # layout its v8dbg_ symbols leave out: class-debug-readers.cc's field
      # offsets, debug-macros.cc's slice functions, bit-fields.h, and
      # instance-types.h, with the V8 headers that hold the few constants
      # Torque does not. scripts/v8-layouts.py turns them into
      # src/debug_info/v8/layouts.gen.rs. Node is configured as its release
      # is, with full ICU, Rust present for Temporal, and clang, because any
      # other configuration shifts instance types; configure's warnings that
      # it turned a feature off fail the build. Only the run_torque target is
      # built, not V8.
      v8Torque = { version, hash }: pkgs.clangStdenv.mkDerivation {
        pname = "v8-torque";
        inherit version;
        src = pkgs.fetchurl {
          url = "https://nodejs.org/dist/v${version}/node-v${version}.tar.xz";
          inherit hash;
        };
        nativeBuildInputs = [ pkgs.python3 pkgs.gnumake pkgs.which pkgs.cargo pkgs.rustc ];
        dontConfigure = true;
        dontFixup = true;
        buildPhase = ''
          runHook preBuild
          patchShebangs configure tools deps/v8/tools
          ./configure --without-npm 2>&1 | tee configure.log
          if grep -iE 'warning|disabl' configure.log; then
            echo "configure did not configure Node as its release" >&2
            exit 1
          fi
          make -C out BUILDTYPE=Release -j$NIX_BUILD_CORES run_torque
          runHook postBuild
        '';
        installPhase = ''
          runHook preInstall
          mkdir -p $out/torque-generated $out/v8
          for file in class-debug-readers.cc debug-macros.cc bit-fields.h instance-types.h; do
            cp out/Release/obj/gen/torque-generated/$file $out/torque-generated/
          done
          (cd deps/v8 && find include src/objects src/common src/execution src/codegen src/sandbox src/builtins src/debug \
            -maxdepth 1 \( -name '*.h' -o -name '*.tq' \) -exec cp --parents {} $out/v8/ \;)
          # What reading an optimized frame's deoptimization translation
          # needs: its opcodes, its operands' encoding, the flag that would
          # compress it, and how a builtin continuation names its builtin.
          (cd deps/v8 && cp --parents src/deoptimizer/translation-opcode.h src/base/vlq.h \
            src/flags/flag-definitions.h src/utils/utils.h $out/v8/)
          # Bytecodes and their operands, for the sizes that map a Sparkplug
          # frame's instruction to its bytecode when no symbol names V8's table.
          (cd deps/v8 && cp --parents src/interpreter/bytecodes.h \
            src/interpreter/bytecode-operands.h $out/v8/)
          cp deps/v8/include/v8-version.h $out/v8/
          runHook postInstall
        '';
      };
      v8Torques = pkgs.linkFarm "v8-torque" [
        { name = "14.6"; path = v8Torque { version = "26.12.1"; hash = "sha256-BT47KWhZttjZP5nvl/qc7BxO4XHHKWgQDGSZeMHb04w="; }; }
        { name = "13.6"; path = v8Torque { version = "24.22.1"; hash = "sha256-T1N5eWKAza6wwvNY/QS2oWzE+336g7WoyRqdljtDUjE="; }; }
        { name = "12.4"; path = v8Torque { version = "22.23.4"; hash = "sha256-e5rcgtYS0KKoY9lsRt4iklUuJQzuUZVuu+AUbEKqyuk="; }; }
      ];

      # GNU Fortran, alone: its wrapper's own gcc and binutils would shadow
      # the shell's.
      gfortran = pkgs.writeShellScriptBin "gfortran" ''
        exec ${pkgs.gfortran15}/bin/gfortran "$@"
      '';

      # LDC, alone: its package would add its runtime's headers and
      # libraries to every C compile and link in the shell.
      ldc2 = pkgs.writeShellScriptBin "ldc2" ''
        exec ${pkgs.ldc}/bin/ldc2 "$@"
      '';

      # GNAT's gnatmake, which runs its own GCC, binder, and linker; the
      # shell's GCC compiles no Ada.
      gnatmake = pkgs.writeShellScriptBin "gnatmake" ''
        PATH=${pkgs.gnat15}/bin:$PATH exec gnatmake "$@"
      '';

      # The crates the tokio fixtures depend on, exactly as their lockfiles
      # name them, one for each tokio release they are built with, so that
      # building them fetches nothing.
      tokioFixtureCrates = pkgs.symlinkJoin {
        name = "tokio-fixture-crates";
        paths = map (lockFile: pkgs.rustPlatform.importCargoLock { inherit lockFile; }) [
          ./tests/fixtures/rust/tokio/Cargo.lock
          ./tests/fixtures/rust/tokio/tokio-1.52/Cargo.lock
        ];
      };

      benchCorpus = pkgs.runCommand "microscope-bench-corpus" {
        src = ./bench/corpus;
        nativeBuildInputs = [ pkgs.gcc rust goStable pkgs.zig ];
        hardeningDisable = [ "all" ];
        dontStrip = true;
        dontFixup = true;
      } ''
        build=$(mktemp -d)
        # Zig has no debug-path remapping; compile from the immutable source.
        cd "$src"
        export HOME="$build/home" XDG_CACHE_HOME="$build/cache"
        mkdir -p "$HOME" "$XDG_CACHE_HOME"
        export SOURCE_DATE_EPOCH=1
        # The stdenv seed and RPATH name $out; neither belongs in corpus bytes.
        export NIX_CFLAGS_COMPILE="-frandom-seed=microscope-bench-corpus"
        export NIX_LDFLAGS=""

        g++ main.cpp -o "$build/cpp" -std=c++20 -O0 -g -gdwarf-5 \
          -march=x86-64 -mtune=generic \
          -ffile-prefix-map="$PWD"=/microscope/bench/corpus
        rustc main.rs -o "$build/rust" --edition=2024 --crate-name bench_corpus \
          --target=x86_64-unknown-linux-gnu -C target-cpu=x86-64 \
          -C opt-level=0 -C debuginfo=2 -C codegen-units=1 \
          --remap-path-prefix="$PWD"=/microscope/bench/corpus

        export GOENV=off GOTOOLCHAIN=local GOPROXY=off GOSUMDB=off
        export GOOS=linux GOARCH=amd64 GOAMD64=v1 CGO_ENABLED=0 GOMAXPROCS=1
        export GOPATH="$build/go-path" GOCACHE="$build/go-cache" GOMODCACHE="$build/go-mod-cache"
        go build -trimpath -buildvcs=false -gcflags='all=-N -l' \
          -ldflags=-buildid= -o "$build/go" main.go

        # FD paths keep compiler-rt's DWARF stable without sharing its caches.
        mkdir "$build/zig-cache" "$build/zig-global-cache"
        exec 3<"$build/zig-cache" 4<"$build/zig-global-cache"
        zig build-exe main.zig -femit-bin="$build/zig" -target x86_64-linux-gnu \
          -mcpu=baseline -O Debug -fno-llvm -fno-strip -j1 \
          --cache-dir /proc/self/fd/3 --global-cache-dir /proc/self/fd/4

        mkdir "$out"
        install -m755 "$build/"{cpp,rust,go,zig} "$out/"
      '';

      default = pkgs.mkShell {
        NIX_HARDENING_ENABLE = "";
        MICROSCOPE_BENCH_CORPUS = "${benchCorpus}";
        # glibc's static libraries, which only statically linked fixtures link
        # against: on the default search path they would shadow the shared C
        # library for every program.
        GLIBC_STATIC_LIBRARIES = "${pkgs.glibc.static}/lib";
        # The shared glibc the fixtures run with, which tests read directly:
        # microscope and its tests may link musl instead.
        GLIBC_LIBRARIES = "${pkgs.glibc}/lib";
        # Ubuntu's glibc builds, one directory for each version, holding its
        # lib and dev directories.
        MICROSCOPE_UBUNTU_GLIBC = "${ubuntuGlibcs}";
        # The CPython interpreters Python support is tested against, one
        # directory each, out of the shell's packages.
        MICROSCOPE_CPYTHON = "${cpython}";
        # The Node.js builds Node support is tested against, one directory
        # each, out of the shell's packages.
        MICROSCOPE_NODE = "${nodes}";
        # V8's Torque output for each supported V8 version, which
        # scripts/v8-layouts.py reads.
        MICROSCOPE_V8_TORQUE = "${v8Torques}";
        # The vendored crates scripts/build-test-programs.sh builds the tokio
        # fixtures against, offline.
        MICROSCOPE_FIXTURE_CRATES = "${tokioFixtureCrates}";
        # The web UI's end-to-end tests drive the browsers nixpkgs builds,
        # which must match the pinned @playwright/test exactly.
        PLAYWRIGHT_BROWSERS_PATH = "${pkgs.playwright-driver.browsers}";
        PLAYWRIGHT_SKIP_VALIDATE_HOST_REQUIREMENTS = "true";
        PLAYWRIGHT_DRIVER_VERSION = pkgs.playwright-driver.version;
        # The certificate authorities that pnpm and other downloads trust.
        # nixpkgs' OpenSSL otherwise looks for a bundle at a path that only
        # some distributions provide (not Fedora's), and --ignore-env drops
        # the host's own setting.
        SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
        NIX_SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
        RUSTFLAGS = "-C link-arg=-fuse-ld=mold -C link-arg=-Wl,--dynamic-linker=${pkgs.glibc}/lib/ld-linux-x86-64.so.2";
        # `just build-static` links microscope against the musl and libunwind
        # this flake pins, not the copies Rust ships, so its C dependencies
        # compile against the headers of the C library they link. Cargo reads
        # the target's RUSTFLAGS only when RUSTFLAGS is unset, which the recipe
        # does.
        CC_x86_64_unknown_linux_musl = "${pkgsStatic.stdenv.cc}/bin/x86_64-unknown-linux-musl-gcc";
        AR_x86_64_unknown_linux_musl = "${pkgsStatic.stdenv.cc.bintools}/bin/x86_64-unknown-linux-musl-ar";
        CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER = "${pkgsStatic.stdenv.cc}/bin/x86_64-unknown-linux-musl-gcc";
        CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS = "-C target-feature=+crt-static -C link-self-contained=no -C link-arg=-fuse-ld=mold -L ${pkgsStatic.llvmPackages.libunwind}/lib";
        MICROSCOPE_MUSL_VERSION = pkgsStatic.musl.version;
        # build.rs compiles the syscall tracer's BPF programs with clang
        # itself, not its wrapper, which adds host flags that a BPF target
        # rejects, against libbpf's BPF headers and the kernel's UAPI
        # headers, whose syscall numbers also generate src/syscalls' tables.
        MICROSCOPE_BPF_CLANG = "${pkgs.llvmPackages.clang-unwrapped}/bin/clang";
        MICROSCOPE_BPF_INCLUDE = "${pkgs.libbpf}/include:${pkgs.linuxHeaders}/include";
        packages = with pkgs; [
          rust
          cargo-fuzz
          cargo-mutants
          cargo-nextest
          just
          gcc
          mold
          clang
          clangLibcxx
          muslGcc
          muslClang
          # Points fixtures built for Ubuntu's glibc at its loader and
          # libraries.
          patchelf
          gdb
          lldb
          # Shares fixtures' debug information through supplementary files,
          # as distributions do.
          dwz
          # The memory cap tests run under asks the user's systemd for a scope.
          systemd
          git
          jq
          strace
          # Profilers for the `just profile-*` recipes.
          perf
          valgrind
          goStable
          zig
          odin
          gfortran
          ldc2
          nim
          gnatmake
          pkg-config
          util-linux
          nodejs_24
          pnpm
          biome
          # Lints the agent protocol's schema and keeps it compatible.
          buf
        ];
      };
    in {
      packages.${system}.bench-corpus = benchCorpus;

      devShells.${system} = {
        inherit default;
        # The default shell with the profilers people read at a screen, kept
        # out of it for their size: samply, hotspot, heaptrack, hyperfine,
        # poop, and KCachegrind.
        profile = default.overrideAttrs (previous: {
          nativeBuildInputs = previous.nativeBuildInputs ++ (with pkgs; [
            samply
            hotspot
            heaptrack
            hyperfine
            poop
            kdePackages.kcachegrind
          ]);
        });
      };

      # The privilege `microscope syscalls` needs for its BPF engine, and
      # `just test-bpf` for its tests: a wrapper around env that raises
      # CAP_BPF and CAP_PERFMON into the ambient set before it runs its
      # command, so they reach microscope through nix develop, systemd-run, and
      # nextest. Only root and the wheel group may run it. A host imports
      # it into its own NixOS or system-manager configuration; neither the
      # dev shell nor the nix store can make a binary with capabilities.
      nixosModules.microscope-bpf = { pkgs, ... }: {
        security.wrappers.microscope-bpf = {
          source = "${pkgs.coreutils}/bin/env";
          owner = "root";
          group = "wheel";
          permissions = "u+rx,g+x";
          capabilities = "cap_bpf,cap_perfmon+ep";
        };
      };
    };
}
