import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';
import 'package:slopd_control/connections.dart';
import 'package:slopd_control/control.dart';
import 'package:slopd_control/identities.dart';

void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();

  testWidgets('Android requires a key choice and saves the chosen order', (
    tester,
  ) async {
    final store = IdentityStore();
    final previous = await store.load();
    try {
      final first = await store.generate('E2E first');
      final second = await store.generate('E2E second');

      await tester.pumpWidget(const MaterialApp(home: ConnectionsPage()));
      await tester.pumpAndSettle();
      expect(
        tester
            .widget<DropdownButtonFormField<String>>(
              find.byType(DropdownButtonFormField<String>),
            )
            .initialValue,
        '',
      );

      Profile? saved;
      await tester.pumpWidget(
        MaterialApp(
          home: Builder(
            builder: (context) => Scaffold(
              body: TextButton(
                onPressed: () async {
                  saved = await Navigator.of(context).push<Profile>(
                    MaterialPageRoute(
                      builder: (_) => ProfilePage(
                        profile: Profile(
                          name: 'E2E server',
                          host: 'identity-test.invalid',
                          user: 'alice',
                          keyId: first.id,
                        ),
                      ),
                    ),
                  );
                },
                child: const Text('Edit'),
              ),
            ),
          ),
        ),
      );
      await tester.tap(find.text('Edit'));
      await tester.pumpAndSettle();
      final add = find.text('Add another key (optional)');
      await tester.ensureVisible(add);
      await tester.tap(add);
      await tester.pumpAndSettle();
      await tester.tap(find.text('E2E second').last);
      await tester.pumpAndSettle();
      await tester.tap(find.byTooltip('Move first key down'));
      await tester.pumpAndSettle();
      await tester.tap(find.text('Save connection'));
      await tester.pumpAndSettle();
      expect(saved?.selectedKeyIds, [second.id, first.id]);
    } finally {
      await tester.pumpWidget(const SizedBox.shrink());
      await store.save(previous);
    }
  });
}
