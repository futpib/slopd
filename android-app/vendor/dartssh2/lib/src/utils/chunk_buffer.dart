import 'dart:typed_data';

/// A buffer that queues chunks of data (typically produced by a socket) and
/// lets callers consume parts or all of it as more becomes available.
///
/// [add] is amortised O(1): incoming chunks are copied into a single
/// growable backing array with a write cursor, which is only reallocated
/// (doubling in size, and compacting away already-consumed bytes in the
/// process) when its trailing free space runs out -- not on every call, as
/// concatenating into a brand new array on every `add()` would do. Growth
/// always allocates a *new* array rather than shifting bytes within the
/// existing one, so nothing this buffer does ever mutates memory that has
/// already been handed out through [view], [consume] or [consumeView]. The
/// backing array is released as soon as the buffer is fully drained, so
/// memory does not accumulate across a long-lived connection.
///
/// Two families of read accessor are exposed, and their names say which is
/// which: [view] and [consumeView] return a VIEW that aliases this buffer's
/// backing storage -- free of an extra allocation, but pinning whatever
/// backing chunk it came from in memory for as long as it's held, and only
/// safe to use where the caller does not need the result to survive
/// independently of this buffer. [consume] (the default, ownership-taking
/// operation) returns a freshly allocated COPY -- an extra allocation, but
/// safe to hand to a caller that may hold onto it indefinitely.
class ChunkBuffer {
  static const int _minCapacity = 4096;

  Uint8List _storage = Uint8List(0);

  /// Index of the first unconsumed byte in [_storage].
  int _start = 0;

  /// One past the index of the last written byte in [_storage].
  int _end = 0;

  /// Appends [chunk] to the buffer.
  ///
  /// Amortised O(1): reuses trailing free space in the backing array when
  /// there is enough of it, and otherwise reallocates geometrically
  /// (doubling), copying across only the bytes still unconsumed -- so the
  /// cost of growing is amortised over the data added since the last
  /// growth rather than the buffer's entire lifetime. This is what keeps
  /// repeated `add()` calls (e.g. while streaming a large SFTP transfer)
  /// from degrading to O(n^2) overall.
  void add(Uint8List chunk) {
    if (chunk.isEmpty) return;

    if (_end + chunk.length <= _storage.length) {
      // Enough trailing room: append in place. This only ever writes into
      // memory that has never been exposed through a read accessor, so it
      // can never invalidate a previously returned result.
      _storage.setRange(_end, _end + chunk.length, chunk);
      _end += chunk.length;
      return;
    }

    // Not enough trailing room: grow (and, as a side effect, compact away
    // already-consumed bytes) into a freshly allocated array. The old
    // array is left untouched, so anything previously handed out via
    // [view]/[consumeView] remains valid.
    final liveLength = length;
    final needed = liveLength + chunk.length;
    var newCapacity = _storage.isEmpty ? _minCapacity : _storage.length * 2;
    while (newCapacity < needed) {
      newCapacity *= 2;
    }

    final newStorage = Uint8List(newCapacity);
    newStorage.setRange(0, liveLength, _storage, _start);
    newStorage.setRange(liveLength, needed, chunk);
    _storage = newStorage;
    _start = 0;
    _end = needed;
  }

  /// Removes [length] bytes (or, if [length] is omitted, all remaining
  /// bytes) from the front of the buffer and returns a freshly allocated
  /// COPY of them.
  ///
  /// Safe to hold onto for as long as the caller likes: the returned array
  /// is independent of this buffer's backing storage, so it is never
  /// affected by later `add()`/`consume()` calls, and it never pins more
  /// memory than the bytes it actually contains. Prefer [consumeView] only
  /// where the result is provably read before anything else about this
  /// buffer matters -- see its doc comment.
  Uint8List consume([int? length]) {
    final n = length ?? this.length;
    _checkRange(n);
    final result = _storage.sublist(_start, _start + n);
    _advance(n);
    return result;
  }

  /// Like [consume], but returns a VIEW into this buffer's backing storage
  /// instead of a copy.
  ///
  /// Cheaper than [consume] (no allocation or copy), but comes with two
  /// caveats: writing to the returned array corrupts this buffer's own
  /// memory, and holding onto the returned array keeps the *entire*
  /// backing chunk it was sliced from resident in memory, not just the
  /// slice itself. Only call this where the result is used and discarded
  /// before anything that might outlive it needs the memory back -- e.g.
  /// consumed synchronously within the current function, never stored
  /// across an `await`.
  Uint8List consumeView([int? length]) {
    final n = length ?? this.length;
    _checkRange(n);
    final result = Uint8List.sublistView(_storage, _start, _start + n);
    _advance(n);
    return result;
  }

  /// Discards the first [length] bytes from the front of the buffer without
  /// allocating anything to hold them. Equivalent to, but cheaper than,
  /// calling [consumeView] and ignoring the result.
  void skip(int length) {
    _checkRange(length);
    _advance(length);
  }

  void _advance(int n) {
    _start += n;
    if (_start == _end) {
      // Fully drained: release the backing array immediately rather than
      // waiting for the next add() to replace it, so an idle period after
      // a large burst doesn't keep a big allocation alive.
      _storage = Uint8List(0);
      _start = 0;
      _end = 0;
    }
  }

  void _checkRange(int n) {
    if (n < 0 || n > length) {
      throw RangeError.range(n, 0, length, 'length');
    }
  }

  void clear() {
    _storage = Uint8List(0);
    _start = 0;
    _end = 0;
  }

  /// The unconsumed bytes currently buffered, as a VIEW into the backing
  /// storage. See [consumeView] for the aliasing/lifetime caveats.
  Uint8List get data => Uint8List.sublistView(_storage, _start, _end);

  /// Returns a VIEW of [length] bytes starting at [start] bytes into the
  /// unconsumed data, without consuming them. See [consumeView] for the
  /// aliasing/lifetime caveats.
  Uint8List view(int start, int length) {
    final from = _start + start;
    final to = from + length;
    if (start < 0 || length < 0 || to > _end) {
      throw RangeError.range(length, 0, this.length - start, 'length');
    }
    return Uint8List.sublistView(_storage, from, to);
  }

  int get length => _end - _start;

  bool get isEmpty => _start == _end;

  bool get isNotEmpty => _start != _end;

  ByteData get byteData => ByteData.sublistView(_storage, _start, _end);

  @override
  String toString() {
    return 'SSHChunkBuffer(length: $length)';
  }
}
