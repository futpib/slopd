import 'package:dartssh2/dartssh2.dart';
import 'package:flutter_secure_storage/flutter_secure_storage.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:slopd_control/control.dart';
import 'package:slopd_control/identities.dart';
import 'package:slopd_control/open_keychain.dart';

void main() {
  test('profile preserves explicit SSH key order', () {
    final profile = Profile(
      name: 'Server',
      host: 'server',
      user: 'alice',
      keyId: 'primary',
      additionalKeyIds: const ['second', 'third'],
    );
    final restored = Profile.fromJson(profile.toJson());
    expect(restored.selectedKeyIds, ['primary', 'second', 'third']);
    expect(
      Profile.fromJson(
        {...profile.toJson()}..remove('additionalKeyIds'),
      ).selectedKeyIds,
      ['primary'],
    );
  });

  TestWidgetsFlutterBinding.ensureInitialized();
  setUp(() => FlutterSecureStorage.setMockInitialValues({}));

  test(
    'external references persist alongside local keys without private material',
    () async {
      final store = IdentityStore();
      final local = await store.generate('Local');
      final reference = OpenKeychainKey(
        providerPackage: 'org.example.provider',
        providerLabel: 'Provider',
        keyId: '1234',
        description: 'External',
        publicKeyString: local.publicKey,
      );
      final external = await store.importExternal(reference);
      await store.importExternal(reference);
      expect((await store.load()).length, 2);
      final saved = await store.get(external.id);
      expect(saved.pem, isEmpty);
      expect(saved.passphrase, isEmpty);
      expect(saved.externalKey!.toJson(), reference.toJson());
      expect(saved.fingerprint, local.fingerprint);
      expect(saved.createIdentities().single.shouldProbe, isTrue);
      expect((await store.get(local.id)).pem, local.pem);
      const profiles = ProfileStore();
      await profiles.save([
        Profile(name: 'Server', host: 'host', user: 'alice', keyId: saved.id),
      ]);
      expect((await profiles.load()).single.keyId, saved.id);
    },
  );

  test('an invalid legacy key does not hide the connection list', () async {
    const store = ProfileStore();
    await store.save([
      Profile(
        name: 'Needs repair',
        host: 'host',
        user: 'alice',
        privateKey: 'invalid',
      ),
    ]);
    final profiles = await store.load();
    expect(profiles.single.name, 'Needs repair');
    expect(profiles.single.privateKey, 'invalid');
  });

  test(
    'generated Ed25519 identities round-trip and duplicate imports reuse them',
    () async {
      final store = IdentityStore();
      final key = await store.generate('Phone');
      final pair = SSHKeyPair.fromPem(key.pem).single;
      expect(pair.type, 'ssh-ed25519');
      expect(key.publicKey, startsWith('ssh-ed25519 '));
      expect(key.fingerprint, startsWith('SHA256:'));
      final duplicate = await store.import('Other name', key.pem);
      expect(duplicate.id, key.id);
      expect((await store.load()).length, 1);
      expect((await store.get(key.id)).pem, key.pem);
      await expectLater(
        store.import('Bad key', 'not a private key'),
        throwsA(anything),
      );
      expect((await store.load()).length, 1);
    },
  );

  test(
    'legacy profile migration deduplicates identities and preserves host trust',
    () async {
      final keys = IdentityStore();
      final key = await keys.generate('Existing');
      await keys.save([]);
      const profiles = ProfileStore();
      final first = Profile(
        name: 'One',
        host: 'one',
        user: 'alice',
        privateKey: key.pem,
      );
      final second = Profile(
        name: 'Two',
        host: 'two',
        user: 'bob',
        privateKey: key.pem,
      );
      await profiles.save([first, second]);
      await profiles.trust(first.hostIdentity, 'pinned');
      final migrated = await profiles.load();
      expect(migrated.map((p) => p.keyId).toSet(), {key.id});
      expect(
        migrated.every((p) => p.privateKey.isEmpty && p.passphrase.isEmpty),
        isTrue,
      );
      expect((await keys.load()).length, 1);
      expect(await profiles.hostKey(first.hostIdentity), 'pinned');
      expect((await profiles.load()).first.keyId, key.id);
      expect((await keys.load()).length, 1);
    },
  );
}
