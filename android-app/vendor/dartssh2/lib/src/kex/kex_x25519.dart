import 'dart:typed_data';

import 'package:dartssh2/src/ssh_errors.dart';
import 'package:dartssh2/src/ssh_kex.dart';
import 'package:dartssh2/src/utils/bigint.dart';
import 'package:dartssh2/src/utils/list.dart';
import 'package:pinenacl/tweetnacl.dart';

class SSHKexX25519 implements SSHKexECDH {
  /// Randomly generated private key.
  final Uint8List privateKey;

  /// Public key computed from the private key.
  @override
  final Uint8List publicKey;

  factory SSHKexX25519() {
    final privateKey = randomBytes(32);
    final publicKey = _ScalarMult.scalseMultBase(privateKey);
    return SSHKexX25519._(
      privateKey: privateKey,
      publicKey: publicKey,
    );
  }

  SSHKexX25519._({required this.privateKey, required this.publicKey});

  @override
  BigInt computeSecret(Uint8List remotePublicKey) {
    // https://tools.ietf.org/html/rfc8731#section-3 MUSTs: "Clients and
    // servers MUST also abort if the length of the received public keys are
    // not the expected lengths." Without this a short or overlong key would
    // reach scalseMult, which reads a fixed 32 bytes regardless.
    if (remotePublicKey.length != _ScalarMult.groupElementLength) {
      throw SSHHandshakeError(
        'Invalid X25519 exchange: peer public key is '
        '${remotePublicKey.length} bytes, expected '
        '${_ScalarMult.groupElementLength}',
      );
    }

    final secret = _ScalarMult.scalseMult(privateKey, remotePublicKey);

    // https://tools.ietf.org/html/rfc8731#section-3 MUSTs: if the computed
    // shared secret is all-zero, abort. Curve25519 has a handful of small
    // order points (see RFC 7748 §6.1); a malicious peer can send one of
    // these as its "public key" to force our scalar multiplication to
    // produce an all-zero output regardless of our private key, which would
    // otherwise let the exchange silently proceed with a known secret.
    var isAllZero = 0;
    for (final byte in secret) {
      isAllZero |= byte;
    }
    if (isAllZero == 0) {
      throw SSHHandshakeError(
        'Invalid X25519 exchange: shared secret is all-zero '
        '(peer public key may be a small-order point)',
      );
    }

    return decodeBigIntWithSign(1, secret);
  }
}

/// Scalar multiplication, Implements curve25519.
class _ScalarMult {
  /// Length of scalar in bytes.
  static final int _scalarLength = 32;

  /// Length of group element in bytes.
  static final int groupElementLength = 32;

  /// Multiplies an integer n by a group element p and returns the resulting
  /// group element.
  static Uint8List scalseMult(Uint8List n, Uint8List p) {
    if (n.length != _scalarLength) {
      throw ArgumentError('n must be 32 bytes long');
    }

    if (p.length != groupElementLength) {
      throw ArgumentError('p must be 32 bytes long');
    }

    final q = Uint8List(_scalarLength);

    TweetNaCl.crypto_scalarmult(q, n, p);

    return q;
  }

  /// Multiplies an integer n by a standard group element and returns the
  /// resulting group element.
  static Uint8List scalseMultBase(Uint8List n) {
    if (n.length != _scalarLength) {
      throw ArgumentError('n must be 32 bytes long');
    }

    final q = Uint8List(_scalarLength);

    TweetNaCl.crypto_scalarmult_base(q, n);

    return q;
  }
}
