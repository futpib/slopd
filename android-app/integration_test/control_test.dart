import 'dart:convert';

import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';
import 'package:slopd_control/control.dart';
import 'package:slopd_control/identities.dart';
import 'package:slopd_control/main.dart';

void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();
  final fixture =
      jsonDecode(const String.fromEnvironment('SLOPD_FIXTURE'))
          as Map<String, dynamic>;

  Profile profile(bool iroh, {String? executable, bool control = true}) =>
      Profile(
        name: iroh ? 'Iroh test' : 'SSH test',
        host: (iroh ? fixture['endpoint'] : fixture['host']) as String,
        port: fixture['port'] as int,
        user: fixture['user'] as String,
        iroh: iroh,
        privateKey: fixture['key'] as String,
        executable: executable ?? fixture['executable'] as String,
        socket: control ? fixture['socket'] as String : '',
      );

  for (final iroh in [false, true]) {
    testWidgets('control slopd over ${iroh ? 'Iroh SSH' : 'ordinary SSH'}', (
      tester,
    ) async {
      String? hostKey;
      final api = await SshControl.connect(profile(iroh), (type, fingerprint) {
        hostKey = '$type $fingerprint';
        return Future.value(true);
      });
      try {
        final status =
            jsonDecode((await api.command(['status', '--json'])).stdout)
                as Map<String, dynamic>;
        expect(status['uptime_secs'], isA<num>());
        expect(status['config_generation'], isA<num>());
        final info =
            jsonDecode((await api.command(['info', '--json'])).stdout)
                as Map<String, dynamic>;
        expect(info['version'], isA<String>());
        expect(info['capabilities'], contains('listen'));
        expect(
          jsonDecode((await api.command(['ps', '--json'])).stdout),
          isList,
        );
        final events = await api.openStream(['listen']);
        try {
          expect(
            jsonDecode(
              await events.lines.first.timeout(const Duration(seconds: 10)),
            ),
            containsPair('subscribed', true),
          );
        } finally {
          events.close();
          await events.done;
        }
        expect(hostKey, isNotEmpty);
      } finally {
        await api.close();
      }

      final shell = await SshControl.connect(
        profile(iroh, executable: '/usr/bin/printf', control: false),
        (_, _) => Future.value(true),
      );
      try {
        final result = await shell.command(['%s', 'general key works']);
        expect(result.stdout, 'general key works');
      } finally {
        await shell.close();
      }
    });
  }

  testWidgets('saved general key connects through the normal UI', (
    tester,
  ) async {
    const profiles = ProfileStore();
    final identities = IdentityStore();
    final previousProfiles = await profiles.load();
    final previousIdentities = await identities.load();
    final direct = profile(false);
    try {
      await profiles.save([direct]);
      final migrated = (await profiles.load()).single;
      expect(migrated.privateKey, isEmpty);
      expect((await identities.get(migrated.keyId)).pem, direct.privateKey);

      await tester.pumpWidget(const SlopdApp());
      for (
        var i = 0;
        i < 100 && find.text(direct.name).evaluate().isEmpty;
        i++
      ) {
        await tester.pump(const Duration(milliseconds: 100));
      }
      await tester.tap(find.text(direct.name));
      for (
        var i = 0;
        i < 200 && find.text('Trust SSH host key?').evaluate().isEmpty;
        i++
      ) {
        await tester.pump(const Duration(milliseconds: 100));
      }
      expect(find.text('Trust SSH host key?'), findsOneWidget);
      await tester.tap(find.text('Trust'));
      for (
        var i = 0;
        i < 200 && find.text('Agents · 0/0').evaluate().isEmpty;
        i++
      ) {
        await tester.pump(const Duration(milliseconds: 100));
      }
      expect(find.text('Agents · 0/0'), findsOneWidget);
      expect(find.text('slopd service online'), findsOneWidget);
      await tester.pageBack();
      await tester.pump(const Duration(seconds: 1));
    } finally {
      await profiles.save(previousProfiles);
      await identities.save(previousIdentities);
      await profiles.forget(direct.hostIdentity);
    }
  });
}
