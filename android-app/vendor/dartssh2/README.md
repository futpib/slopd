# dartssh2 authentication patch

Source: dartssh2 4.1.0 from https://pub.dev/packages/dartssh2/versions/4.1.0.
The library and license are retained unchanged except for `_startAuthentication`
in `lib/src/ssh_client.dart`: send the SSH `none` request first, then let the
existing failure handler select among the authentication methods offered by the
server. This avoids requesting passwords or external-key signatures before
knowing whether the server accepts them, and also permits passwordless accounts.

This is a local dependency because 4.1.0 exposes no method-discovery or ordering
option. When updating it, retain this behavior until upstream provides it.
The application's real SSH authentication tests cover password, keyboard
interactive without a key, key authentication, and password changes.
