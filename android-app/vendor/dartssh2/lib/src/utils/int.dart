import 'dart:typed_data';

/// True on platforms where [int] cannot represent every integer in the
/// 64-bit range exactly.
///
/// dart2js and dartdevc back [int] with a JavaScript `number` (an IEEE 754
/// double), which only has 53 bits of mantissa: integers above 2^53
/// (`9007199254740992`, one more than `Number.MAX_SAFE_INTEGER`) start
/// silently losing precision. The Dart VM's [int] is a real 64-bit
/// two's-complement integer and has no such limit.
///
/// `identical(1, 1.0)` is the standard way to detect this at runtime: `1`
/// and `1.0` are distinct values (an [int] and a [double]) on the VM, but
/// collapse to the same JavaScript number when compiled to JS.
const bool kIntIsImprecise = identical(1, 1.0);

/// A pair of 32-bit unsigned words making up a 64-bit value, most
/// significant first.
typedef Uint32Pair = ({int high, int low});

/// Combines a `(high, low)` pair of 32-bit unsigned words -- each in
/// `0..0xFFFFFFFF` -- into the 64-bit integer they represent.
///
/// On the Dart VM, where [int] is a full 64-bit two's complement integer,
/// this is exact for the entire 64-bit range, including reproducing a
/// negative [int] for values with bit 63 set (matching what
/// `ByteData.getUint64` would have returned).
///
/// On dart2js/dartdevc (see [kIntIsImprecise]), a value that cannot be
/// represented exactly throws [UnsupportedError] instead of silently
/// rounding. This library uses 64-bit values as SFTP file sizes and
/// offsets; a silently truncated offset risks corrupting a file transfer,
/// so a loud failure was chosen over a documented precision-loss caveat.
int combineUint32Pair(int high, int low) {
  // These JavaScript-only guards run in the Chrome test job. The VM coverage
  // upload cannot execute them because kIntIsImprecise is false there.
  // coverage:ignore-start
  if (kIntIsImprecise) {
    // Exact iff the combined value is <= 2^53. Checked without ever
    // computing `high * 0x100000000`, since that multiplication is exactly
    // the operation that would silently lose precision.
    final fitsExactly = high < 0x200000 || (high == 0x200000 && low == 0);
    if (!fitsExactly) {
      throw UnsupportedError(
        'Cannot represent the 64-bit value 0x'
        '${high.toRadixString(16)}${low.toRadixString(16).padLeft(8, '0')} '
        'exactly: it needs more than 53 bits, and this program is compiled '
        'to JavaScript, where int is backed by a double.',
      );
    }
  }
  // coverage:ignore-end
  return high * 0x100000000 + low;
}

/// The inverse of [combineUint32Pair]: splits a 64-bit integer into the
/// `(high, low)` pair of 32-bit unsigned words that [ByteData.setUint64]
/// would have written for it.
///
/// Exact for the full 64-bit range (including negative, i.e. bit-63-set,
/// values) on the Dart VM. On dart2js/dartdevc, throws [UnsupportedError]
/// for a negative value or one above 2^53, for the same reason described on
/// [combineUint32Pair].
Uint32Pair splitUint32Pair(int value) {
  // These JavaScript-only guards run in the Chrome test job. The VM coverage
  // upload cannot execute them because kIntIsImprecise is false there.
  // coverage:ignore-start
  if (kIntIsImprecise) {
    if (value < 0 || value > 9007199254740992 /* 2^53 */) {
      throw UnsupportedError(
        'Cannot represent $value exactly as a 64-bit value: this program '
        'is compiled to JavaScript, where int is backed by a double and '
        'only represents integers exactly up to 2^53.',
      );
    }
    // Division/multiplication by a power of two is exact in binary
    // floating point, so this loses no precision for values in range.
    final high = value ~/ 0x100000000;
    final low = value - high * 0x100000000;
    return (high: high, low: low);
  }
  // coverage:ignore-end
  // Native VM: int is a full 64-bit two's complement integer. `>>` is an
  // arithmetic (sign-extending) shift, and `& 0xFFFFFFFF` keeps only the
  // low 32 bits of the result, so this reproduces the same bit pattern
  // `setUint64` would have written, including for negative values.
  final high = (value >> 32) & 0xFFFFFFFF;
  final low = value & 0xFFFFFFFF;
  return (high: high, low: low);
}

/// 64-bit unsigned integer access for [ByteData], implemented as a pair of
/// 32-bit accesses.
///
/// `ByteData.getUint64`/`setUint64` throw `UnsupportedError` when compiled
/// to JavaScript (dart2js/dartdevc): "Uint64 accessor not supported by
/// dart2js." Every 64-bit field this library puts on the wire (AEAD nonces,
/// SFTP sizes/offsets/attributes) goes through the members below instead,
/// so the library keeps working when compiled to JS.
extension Uint64ByteData on ByteData {
  /// Reads a 64-bit unsigned integer at [byteOffset] and combines it into a
  /// single [int]. See [combineUint32Pair] for the exactness guarantees.
  int getUint64Split(int byteOffset, [Endian endian = Endian.big]) {
    final int high, low;
    if (endian == Endian.big) {
      high = getUint32(byteOffset, endian);
      low = getUint32(byteOffset + 4, endian);
    } else {
      low = getUint32(byteOffset, endian);
      high = getUint32(byteOffset + 4, endian);
    }
    return combineUint32Pair(high, low);
  }

  /// Writes [value] as a 64-bit unsigned integer at [byteOffset]. See
  /// [splitUint32Pair] for the exactness guarantees.
  void setUint64Split(
    int byteOffset,
    int value, [
    Endian endian = Endian.big,
  ]) {
    final pair = splitUint32Pair(value);
    if (endian == Endian.big) {
      setUint32(byteOffset, pair.high, endian);
      setUint32(byteOffset + 4, pair.low, endian);
    } else {
      setUint32(byteOffset, pair.low, endian);
      setUint32(byteOffset + 4, pair.high, endian);
    }
  }

  /// Adds [addend] to the 64-bit unsigned integer stored at [byteOffset],
  /// wrapping modulo 2^64, without ever combining the 64-bit value into a
  /// single [int].
  ///
  /// Used for the AES-GCM nonce counter (RFC 5647), which is derived from
  /// key material and so routinely has its top bit set. Going through
  /// [combineUint32Pair]/[splitUint32Pair] for that would trip the
  /// two's-complement and precision handling above for no reason -- plain
  /// word-at-a-time addition with carry is exact on every platform, since
  /// no intermediate value ever exceeds 33 bits.
  void addToUint64Split(
    int byteOffset,
    int addend, [
    Endian endian = Endian.big,
  ]) {
    final addendWords = splitUint32Pair(addend);
    final highOffset = endian == Endian.big ? byteOffset : byteOffset + 4;
    final lowOffset = endian == Endian.big ? byteOffset + 4 : byteOffset;

    final low = getUint32(lowOffset, endian);
    final high = getUint32(highOffset, endian);

    var newLow = low + addendWords.low;
    var carry = 0;
    if (newLow > 0xFFFFFFFF) {
      newLow -= 0x100000000;
      carry = 1;
    }

    var newHigh = high + addendWords.high + carry;
    if (newHigh > 0xFFFFFFFF) {
      newHigh -= 0x100000000;
    }

    setUint32(lowOffset, newLow, endian);
    setUint32(highOffset, newHigh, endian);
  }
}

extension IntX on int {
  /// Returns a [Uint8List] with the bytes of the integer encoded in [endian].
  Uint8List toUint32([Endian endian = Endian.big]) {
    final result = ByteData(4);
    result.setUint32(0, this, endian);
    return result.buffer.asUint8List();
  }

  /// Returns a [Uint8List] with the bytes of the integer encoded in [endian].
  ///
  /// Implemented as two 32-bit writes rather than [ByteData.setUint64],
  /// which throws `UnsupportedError` when compiled to JavaScript. See
  /// [Uint64ByteData.setUint64Split] for the exactness guarantees.
  Uint8List toUint64([Endian endian = Endian.big]) {
    final result = ByteData(8);
    result.setUint64Split(0, this, endian);
    return result.buffer.asUint8List();
  }

  /// Returns the octal representation of this integer.
  String toOctal() {
    return toRadixString(8);
  }

  /// Returns a colon-separated hex representation of this integer.
  String toColonHex() {
    return toRadixString(16)
        .padLeft(8, '0')
        .replaceAllMapped(RegExp(r'(..)'), (match) => ':${match[1]}');
  }
}
