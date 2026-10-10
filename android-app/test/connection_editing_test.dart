import 'package:flutter/material.dart';
import 'package:flutter_secure_storage/flutter_secure_storage.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:slopd_control/connections.dart';
import 'package:slopd_control/control.dart';
import 'package:slopd_control/identities.dart';

void main() {
  setUp(() => FlutterSecureStorage.setMockInitialValues({}));

  testWidgets(
    'no-key quick connect selection survives returning from key management',
    (tester) async {
      await IdentityStore().generate('Phone');
      await tester.pumpWidget(const MaterialApp(home: ConnectionsPage()));
      await tester.pumpAndSettle();
      await tester.tap(find.byType(DropdownButtonFormField<String>));
      await tester.pumpAndSettle();
      await tester.tap(find.text('Password / no SSH key').last);
      await tester.pumpAndSettle();
      await tester.tap(find.byTooltip('SSH keys'));
      await tester.pumpAndSettle();
      await tester.pageBack();
      await tester.pumpAndSettle();
      expect(
        tester
            .widget<DropdownButtonFormField<String>>(
              find.byType(DropdownButtonFormField<String>),
            )
            .initialValue,
        '',
      );
    },
  );

  testWidgets(
    'connection back preserves edits unless discarded, save bypasses prompt',
    (tester) async {
      await IdentityStore().generate('Phone');
      final original = Profile(name: 'Server', host: 'server', user: 'alice');
      Profile? saved;
      await tester.pumpWidget(
        MaterialApp(
          home: Builder(
            builder: (context) => Scaffold(
              body: TextButton(
                onPressed: () async {
                  saved = await Navigator.of(context).push<Profile>(
                    MaterialPageRoute(
                      builder: (_) => ProfilePage(profile: original),
                    ),
                  );
                },
                child: const Text('Edit'),
              ),
            ),
          ),
        ),
      );
      Future<void> open() async {
        await tester.tap(find.text('Edit'));
        await tester.pumpAndSettle();
      }

      await open();
      await tester.pageBack();
      await tester.pumpAndSettle();
      expect(find.byType(AlertDialog), findsNothing);
      expect(find.text('Edit'), findsOneWidget);
      await open();
      final name = find.widgetWithText(
        TextFormField,
        'Display name (optional)',
      );
      await tester.enterText(name, 'Changed');
      await tester.pump();
      await tester.pageBack();
      await tester.pumpAndSettle();
      expect(find.text('Discard connection changes?'), findsOneWidget);
      await tester.tap(find.text('Keep editing'));
      await tester.pumpAndSettle();
      expect(find.text('Changed'), findsOneWidget);
      await tester.tap(find.text('Save connection'));
      await tester.pumpAndSettle();
      expect(saved?.name, 'Changed');
      expect(find.byType(AlertDialog), findsNothing);
      await open();
      await tester.tap(find.text('Iroh'));
      await tester.pump();
      await tester.pageBack();
      await tester.pumpAndSettle();
      await tester.tap(find.text('Discard'));
      await tester.pumpAndSettle();
      expect(saved, isNull);
      expect(find.text('Edit'), findsOneWidget);
    },
  );
}
