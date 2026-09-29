# Android signing key

`nix build path:.#android-apk` signs the APK with the key in this directory:

- `yolab.jks` — the keystore, key alias `yolab`
- `password` — its password, on the first line

Both are ignored by git and never committed. They are copied into the Nix store when the APK is built.

Create them once:

```sh
keytool -genkeypair -keystore homelab/android-key/yolab.jks -alias yolab \
  -keyalg RSA -keysize 4096 -validity 10000 -dname "CN=YoLab"
printf '%s\n' 'the-password-you-chose' > homelab/android-key/password
```

Keep a backup: Android only installs an update signed with the same key.

Without these files the build signs with a throwaway key, so its APK installs but cannot update one signed with this key. A git-flake build (`.#`) does not see ignored files; pass `--override-input yolab-android-key path:/where/the/key/lives`.
