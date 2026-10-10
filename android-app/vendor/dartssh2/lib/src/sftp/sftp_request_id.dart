class SftpRequestId {
  static const max = 0xFFFFFFFF;

  /// Creates a new sequential request id generator.
  ///
  /// [initial] seeds the internal counter. It's exposed mainly so tests can
  /// exercise the wraparound behavior without calling [next] billions of
  /// times.
  SftpRequestId({int initial = 0}) : _id = initial % max;

  /// Kept in the same range as the values [next] returns (`0..max-1`), so it
  /// never grows unbounded. This avoids losing precision on platforms where
  /// `int` is backed by a double (e.g. compiled to JavaScript), where values
  /// past 2^53 can no longer be represented exactly.
  int _id;

  int get next {
    final current = _id;
    _id = (_id + 1) % max;
    return current;
  }
}
