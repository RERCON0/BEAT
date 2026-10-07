# Release packages

BEAT is licensed under [GNU GPL v3 or later](../LICENSE), like SNATCH. Bundled
components retain their own licenses, collected in `COMPONENT-NOTICES.txt`.
The source for each published executable is linked by its release tag and
signed manifest. The repository must be accessible to recipients of that build.

## Download and verify

Starting with 0.2.1, the portable release ZIP is signed with **Ed25519**.
The signature covers the manifest and SHA-256 hashes of `beat.exe`, the package
README, license and component notices. The manifest records the exact source
commit/tree, tracked input hashes, compiler, lockfile and PE properties.

Get the verification script and key from a trusted BEAT checkout, not from an
untrusted ZIP alone. Install Python 3.13+ and OpenSSL 3 (included with Git for
Windows), then run from the checkout:

```powershell
python -B scripts/release.py verify .\beat-windows-x64.zip
```

Verification does not extract the package or run its EXE. It rejects modified
files, a replacement key, malformed manifests, unexpected ZIP entries and
oversized contents. Extract and run `beat.exe` after successful verification.
The trusted [public key](../release/public-key.pem) has this SHA-256 fingerprint
of its Ed25519 SubjectPublicKeyInfo DER:

```text
2aa01b21cad644b14c7b3dfde3f5d51a44c14c6a6d78438879008da5de717bfa
```

> [!IMPORTANT]
> Ed25519 package signing is **not Windows Authenticode**. SmartScreen can
> still display a warning. If you trust the source, select **More info → Run
> anyway**. The standalone EXE and CI artifacts do not carry this package signature.

Version 0.2.0 was published with hashes and unsigned build metadata. Signing
is not retroactively claimed for that release. A signature establishes the
publisher's approval and package integrity; it does not prove an absence of bugs.

## Publisher build

Use a clean committed Windows checkout, the pinned Rust toolchain, MSVC,
Windows SDK, CMake and OpenSSL 3. Run formatting, Clippy, Rust/Python tests,
documentation/translation checks, RustSec audit and the full-history secret
scan; both workflows must pass for the exact commit being released.

The publisher's private key stays outside Git and CI, with access restricted
to the current Windows account. Keep a separate protected backup. Do not rotate
or regenerate it for each release. The following command is for an independent
publisher's **first** key only; it refuses to overwrite existing keys:

```powershell
python -B scripts/release.py keygen --private-key "$env:LOCALAPPDATA\BEAT\release-signing\beat-private.pem"
```

To build and sign with the existing key:

```powershell
python -B scripts/release.py build --private-key "$env:LOCALAPPDATA\BEAT\release-signing\beat-private.pem"
python -B scripts/release.py verify .\dist\beat-windows-x64.zip
```

The builder uses a fresh target directory, rejects external compiler/profile
and CMake toolchain overrides, verifies the repository did not change while
compiling, and verifies the ZIP before atomic publication. The committed
[MSVC CMake initialization](../.cargo/opus-msvc.cmake) keeps bundled libopus on
the same static runtime as Rust. `CMAKE` may locate the installed CMake executable.
Build tools, OS, linker and registry dependencies still have to be trusted;
this is not a claim of bit-for-bit reproducibility or machine attestation.

Create a new annotated version tag on the verified commit, push it and upload
the verified ZIP with release notes. Do not move existing release tags. Retain
the license and notices when redistributing the package. CI only builds unsigned
candidates; it never receives the production private key.

## Русский

С версии 0.2.1 ZIP подписан Ed25519. Проверка командой `verify` использует ключ
из доверенного checkout, проверяет подпись и хеши всех файлов, не запускает EXE
и не распаковывает архив. Ключ внутри скачанного ZIP сам по себе не является
доказательством издателя. Приватный ключ хранится вне репозитория и CI.

Это не Authenticode: SmartScreen может предупреждать как раньше. Если вы
доверяете источнику, выберите «Подробнее → Выполнить в любом случае».
BEAT распространяется по GPL v3 или новее; лицензии встроенных компонентов
сохраняются отдельно. Исходники соответствующей сборки должны быть доступны
получателям. Версия 0.2.0 не была подписана; её статус не меняется задним числом.
