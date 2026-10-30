# RPM spec for Fedora. Build with: rpmbuild -bb packaging/meersh.spec --define "_sourcedir $PWD"
Name:           meersh
Version:        0.1.0
Release:        1%{?dist}
Summary:        Object oriented shell with language models as pipeline stages
License:        AGPL-3.0-or-later
URL:            https://github.com/ribalba/meersh
BuildRequires:  cargo rust

%description
meersh passes typed records between commands, lets language models work on them as
pipeline stages and checks every action of a model against ownership and policy.

%build
cd %{_sourcedir}
cargo build --release --workspace

%install
cd %{_sourcedir}
make install DESTDIR=%{buildroot} PREFIX=%{_prefix}

%files
%{_bindir}/meersh
%{_bindir}/meer-box
%{_datadir}/doc/meersh/config.example.toml
