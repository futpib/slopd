import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_secure_storage/flutter_secure_storage.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:slopd_control/control.dart';
import 'package:slopd_control/identities.dart';
import 'package:slopd_control/keys_page.dart';

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();
  const channel = MethodChannel('slopd/openkeychain');
  final messenger =
      TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger;
  setUp(() => FlutterSecureStorage.setMockInitialValues({}));
  tearDown(() => messenger.setMockMethodCallHandler(channel, null));

  testWidgets(
    'missing provider and cancellation leave local identities intact',
    (tester) async {
      final key = await IdentityStore().generate('Local');
      messenger.setMockMethodCallHandler(channel, (_) async => []);
      await tester.pumpWidget(const MaterialApp(home: KeysPage()));
      await tester.pumpAndSettle();
      await tester.tap(find.text('Add from OpenKeychain'));
      await tester.pumpAndSettle();
      expect(
        find.textContaining('No OpenKeychain provider is installed'),
        findsOneWidget,
      );
      messenger.setMockMethodCallHandler(channel, (call) async {
        if (call.method == 'listProviders') {
          return [
            {'packageName': 'provider', 'label': 'Provider'},
          ];
        }
        throw PlatformException(code: 'openkeychain_cancelled');
      });
      await tester.tap(find.text('Add from OpenKeychain'));
      await tester.pumpAndSettle();
      expect((await IdentityStore().load()).single.id, key.id);
      expect(find.text('Add from OpenKeychain'), findsOneWidget);
      expect(find.textContaining('openkeychain_cancelled'), findsNothing);
    },
  );

  testWidgets('external key can be selected, copied and safely removed', (
    tester,
  ) async {
    final store = IdentityStore();
    final local = await store.generate('Local');
    final calls = <String>[];
    messenger.setMockMethodCallHandler(channel, (call) async {
      calls.add(call.method);
      switch (call.method) {
        case 'listProviders':
          return [
            {'packageName': 'provider', 'label': 'Provider'},
            {'packageName': 'other', 'label': 'Other'},
          ];
        case 'selectKey':
          expect((call.arguments as Map)['providerPackage'], 'provider');
          return {'keyId': '42', 'description': 'External'};
        case 'getSshPublicKey':
          return local.publicKey;
        default:
          fail('Unexpected provider call: ${call.method}');
      }
    });
    await tester.pumpWidget(const MaterialApp(home: KeysPage()));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Add from OpenKeychain'));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Provider'));
    await tester.pumpAndSettle();
    expect(find.text('Copy public key'), findsOneWidget);
    await tester.tap(find.text('Close'));
    await tester.pumpAndSettle();
    final key = (await store.load()).singleWhere((k) => k.externalKey != null);
    const profiles = ProfileStore();
    await profiles.save([
      Profile(name: 'Server', host: 'host', user: 'alice', keyId: key.id),
    ]);
    Future<void> remove() async {
      final menu = find.descendant(
        of: find.widgetWithText(ListTile, 'External'),
        matching: find.byType(PopupMenuButton<String>),
      );
      await tester.ensureVisible(menu);
      await tester.pumpAndSettle();
      await tester.tap(menu);
      await tester.pumpAndSettle();
      await tester.tap(find.text('Delete').last);
      await tester.pumpAndSettle();
    }

    await remove();
    expect(find.textContaining('This key is used by Server'), findsOneWidget);
    expect((await store.load()).length, 2);
    await profiles.save([]);
    await remove();
    expect(
      find.textContaining('The private key stays in Provider'),
      findsOneWidget,
    );
    await tester.tap(find.text('Delete').last);
    await tester.pumpAndSettle();
    expect((await store.load()).single.id, local.id);
    expect(calls, ['listProviders', 'selectKey', 'getSshPublicKey']);
  });
}
