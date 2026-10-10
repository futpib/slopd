import 'dart:convert';
import 'dart:typed_data';

import 'package:crypto/crypto.dart';
import 'package:dartssh2/dartssh2.dart';
import 'package:flutter_secure_storage/flutter_secure_storage.dart';
import 'package:pinenacl/ed25519.dart' as ed25519;

import 'open_keychain.dart';

class Identity {
  const Identity(this.id, this.name, this.pem, this.passphrase, this.publicKey)
    : externalKey = null;
  Identity.external(OpenKeychainKey key)
    : id = 'openkeychain:${key.providerPackage}:${key.keyId}',
      name = key.description,
      pem = '',
      passphrase = '',
      publicKey = key.publicKeyString,
      externalKey = key;
  final String id, name, pem, passphrase, publicKey;
  final OpenKeychainKey? externalKey;
  String get label =>
      externalKey == null ? name : '$name · ${externalKey!.providerLabel}';
  List<SSHIdentity> createIdentities() => externalKey == null
      ? SSHKeyPair.fromPem(pem, passphrase.isEmpty ? null : passphrase)
      : [externalKey!.createIdentity()];
  String get fingerprint =>
      'SHA256:${base64Encode(sha256.convert(parseOpenSshPublicKey(publicKey).blob).bytes).replaceAll('=', '')}';
  Map<String, dynamic> toJson() => {
    'id': id,
    'name': name,
    'pem': pem,
    'passphrase': passphrase,
    'publicKey': publicKey,
    if (externalKey != null) 'externalKey': externalKey!.toJson(),
  };
  factory Identity.fromJson(Map<String, dynamic> json) =>
      json['externalKey'] != null
      ? Identity.external(
          OpenKeychainKey.fromJson(json['externalKey'] as Map<String, dynamic>),
        )
      : Identity(
          json['id'],
          json['name'],
          json['pem'],
          json['passphrase'],
          json['publicKey'],
        );
}

class IdentityStore {
  static const storage = FlutterSecureStorage();
  Future<List<Identity>> load() async {
    final value = await storage.read(key: 'identities');
    return value == null
        ? []
        : (jsonDecode(value) as List)
              .map((v) => Identity.fromJson(v as Map<String, dynamic>))
              .toList();
  }

  Future<void> save(List<Identity> keys) => storage.write(
    key: 'identities',
    value: jsonEncode(keys.map((k) => k.toJson()).toList()),
  );
  Future<Identity> get(String id) async {
    final keys = await load();
    return keys.firstWhere(
      (k) => k.id == id,
      orElse: () => throw StateError('SSH key is missing. Choose another key.'),
    );
  }

  Future<Identity> import(
    String name,
    String pem, [
    String passphrase = '',
  ]) async {
    final pairs = SSHKeyPair.fromPem(
      pem,
      passphrase.isEmpty ? null : passphrase,
    );
    if (pairs.length != 1) {
      throw const FormatException('Import one SSH private key at a time.');
    }
    final pair = pairs.single;
    final blob = pair.toPublicKey().encode();
    final id = sha256.convert(blob).toString();
    final keys = await load();
    final existing = keys.where((k) => k.id == id).firstOrNull;
    if (existing != null) return existing;
    final key = Identity(
      id,
      name,
      pem,
      passphrase,
      '${pair.type} ${base64Encode(blob)} ${name.replaceAll(RegExp(r'[\r\n]'), ' ')}',
    );
    await save([...keys, key]);
    return key;
  }

  Future<Identity> generate(String name) async {
    final signer = ed25519.SigningKey.generate();
    final pair = OpenSSHEd25519KeyPair(
      Uint8List.fromList(signer.verifyKey.asTypedList),
      Uint8List.fromList(signer.asTypedList),
      name,
    );
    return import(name, pair.toPem());
  }

  Future<Identity> importExternal(OpenKeychainKey reference) async {
    reference.createIdentity();
    final key = Identity.external(reference);
    final keys = await load();
    await save([...keys.where((existing) => existing.id != key.id), key]);
    return key;
  }
}
