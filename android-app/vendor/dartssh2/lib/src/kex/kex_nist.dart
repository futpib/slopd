import 'dart:typed_data';

import 'package:dartssh2/src/kex/private_scalar.dart';
import 'package:dartssh2/src/ssh_errors.dart';
import 'package:dartssh2/src/ssh_kex.dart';
import 'package:pointycastle/ecc/curves/secp256r1.dart';
import 'package:pointycastle/ecc/curves/secp384r1.dart';
import 'package:pointycastle/ecc/curves/secp521r1.dart';
import 'package:pointycastle/pointycastle.dart';

/// The Elliptic Curve Diffie-Hellman (ECDH) key exchange method generates a
/// shared secret from an ephemeral local elliptic curve private key and
/// ephemeral remote elliptic curve public key.
class SSHKexNist implements SSHKexECDH {
  /// The elliptic curve domain parameters.
  final ECDomainParameters curve;

  /// The length of the shared secret in bytes.
  final int secretBits;

  /// Secret random number.
  late final BigInt privateKey;

  /// Public key.
  @override
  late final Uint8List publicKey;

  SSHKexNist({required this.curve, required this.secretBits}) {
    privateKey = generateEcPrivateScalar(curve.n);
    final c = curve.G * privateKey;
    publicKey = c!.getEncoded(false);
  }

  SSHKexNist.p256() : this(curve: ECCurve_secp256r1(), secretBits: 256);

  SSHKexNist.p384() : this(curve: ECCurve_secp384r1(), secretBits: 384);

  SSHKexNist.p521() : this(curve: ECCurve_secp521r1(), secretBits: 521);

  /// Compute shared secret.
  @override
  BigInt computeSecret(Uint8List remotePublicKey) {
    final remotePoint = _decodeAndValidatePoint(remotePublicKey);

    // The multiplication itself can only produce the point at infinity when
    // the remote point lies in a small subgroup that the on-curve check
    // above didn't already exclude (e.g. mixed with a malicious privateKey),
    // so this is checked defensively rather than assumed impossible.
    final sharedPoint = remotePoint * privateKey;
    if (sharedPoint == null || sharedPoint.isInfinity) {
      throw SSHHandshakeError(
        'Invalid ECDH exchange: shared secret is the point at infinity',
      );
    }

    final x = sharedPoint.x?.toBigInteger();
    if (x == null) {
      throw SSHHandshakeError(
        'Invalid ECDH exchange: shared secret has no x-coordinate',
      );
    }
    return x;
  }

  /// Decodes [encodedPoint] and validates it per
  /// https://tools.ietf.org/html/rfc5656#section-4 and RFC 4253 §8's
  /// requirement that a peer-supplied public value be checked before use.
  ///
  /// pointycastle's [ECCurve.decodePoint] does not itself guarantee the
  /// result is a valid point: for compressed encodings it derives y from x
  /// via the curve equation (so it is on-curve by construction), but for the
  /// uncompressed encoding (0x04) -- the format actually used by SSH -- it
  /// simply builds an [ECPoint] from the raw x/y coordinates without
  /// checking they satisfy the curve equation at all. A crafted point that
  /// decodes successfully but is not on the curve (invalid-curve attack) can
  /// leak bits of our private key when multiplied. So the on-curve check
  /// below is required, not optional; it was verified against pointycastle
  /// 4.0.0's `lib/ecc/ecc_base.dart` and `lib/ecc/ecc_fp.dart` sources.
  ECPoint _decodeAndValidatePoint(Uint8List encodedPoint) {
    ECPoint? point;
    try {
      point = curve.curve.decodePoint(encodedPoint);
    } on ArgumentError catch (e) {
      throw SSHHandshakeError('Invalid ECDH public key: $e');
    }

    if (point == null || point.isInfinity) {
      throw SSHHandshakeError(
        'Invalid ECDH public key: point at infinity',
      );
    }

    final x = point.x;
    final y = point.y;
    if (x == null || y == null) {
      throw SSHHandshakeError('Invalid ECDH public key: missing coordinates');
    }

    // y^2 == x^3 + a*x + b (mod p)
    final lhs = y.square();
    final rhs = (x.square() * x) + (curve.curve.a! * x) + curve.curve.b!;
    if (lhs != rhs) {
      throw SSHHandshakeError(
        'Invalid ECDH public key: point is not on the curve',
      );
    }

    return point;
  }
}
