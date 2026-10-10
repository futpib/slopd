import 'dart:math';

import 'dart:typed_data';

final _secureRandom = Random.secure();

Uint8List randomBytes(int length) {
  final bytes = Uint8List(length);
  for (var i = 0; i < length; i++) {
    bytes[i] = _secureRandom.nextInt(256);
  }
  return bytes;
}

extension ListX<T> on List<T> {
  bool equals(List<T> other) {
    if (other.length != length) return false;
    for (int i = 0; i < length; i++) {
      if (this[i] != other[i]) return false;
    }
    return true;
  }
}

/// Compares [a] and [b] for equality without leaking timing information
/// about the contents of either list.
///
/// Every byte is XORed and accumulated with a bitwise OR; there is a single
/// comparison at the end and no data-dependent branch or early return based
/// on byte contents, unlike [ListX.equals]. Intended for comparing secret
/// values such as MACs, where an early-exit comparison lets an attacker
/// learn how many leading bytes of their guess were correct from response
/// timing.
bool constantTimeEquals(Uint8List a, Uint8List b) {
  if (a.length != b.length) return false;
  var diff = 0;
  for (var i = 0; i < a.length; i++) {
    diff |= a[i] ^ b[i];
  }
  return diff == 0;
}
