Name:           anodizer
Version:        {{ Version }}
Release:        1%{?dist}
Summary:        A Rust-native release automation tool
License:        MIT
URL:            https://github.com/tj-smith47/anodizer
Source0:        %{name}-%{version}-source.tar.gz

%description
Anodizer is a release automation tool for Rust projects, inspired by
GoReleaser. It handles building, packaging, publishing, and announcing
releases across multiple platforms and package managers.

%prep
%autosetup -n {{ SourcePrefix }}

%build
cargo build --release

%install
install -D -m 0755 target/release/%{name} %{buildroot}%{_bindir}/%{name}
install -D -m 0644 LICENSE %{buildroot}%{_datadir}/doc/%{name}/LICENSE
install -D -m 0644 README.md %{buildroot}%{_datadir}/doc/%{name}/README.md

%files
%{_bindir}/%{name}
%{_datadir}/doc/%{name}/LICENSE
%{_datadir}/doc/%{name}/README.md

%changelog
