import 'package:dartssh2/dartssh2.dart';
import 'package:flutter/material.dart';
import 'package:flutter_secure_storage/flutter_secure_storage.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:slopd_control/authentication.dart';
import 'package:slopd_control/connections.dart';
import 'package:slopd_control/control.dart';

void main() {
  testWidgets(
    'informational challenges acknowledge immediately, replace and cancel',
    (tester) async {
      late BuildContext context;
      await tester.pumpWidget(
        MaterialApp(
          home: Builder(
            builder: (value) {
              context = value;
              return const Scaffold();
            },
          ),
        ),
      );
      final dialog = AuthenticationDialog(context, 'alice@server');
      expect(
        await dialog.request(
          SSHUserInfoRequest('Push approval', 'Approve on your device', []),
          false,
        ),
        isEmpty,
      );
      await tester.pumpAndSettle();
      expect(find.text('Approve on your device'), findsOneWidget);
      expect(find.text('Continue'), findsNothing);
      final response = dialog.request(
        SSHUserInfoRequest('Code', '', [SSHUserInfoPrompt('OTP', false)]),
        false,
      );
      await tester.pumpAndSettle();
      expect(find.text('Push approval'), findsNothing);
      expect(dialog.cancellation.isCancelled, isFalse);
      await tester.tap(find.text('Cancel'));
      expect(await response, isNull);
      expect(dialog.cancellation.isCancelled, isTrue);
      await tester.pumpAndSettle();
      final info = AuthenticationDialog(context, 'alice@server');
      await info.request(SSHUserInfoRequest('Push approval', '', []), false);
      await tester.pumpAndSettle();
      await tester.binding.handlePopRoute();
      await tester.pumpAndSettle();
      expect(info.cancellation.isCancelled, isTrue);
    },
  );

  testWidgets('closing a trust dialog rejects stale acceptance', (
    tester,
  ) async {
    late AuthenticationDialog dialog;
    await tester.pumpWidget(
      MaterialApp(
        home: Builder(
          builder: (context) {
            dialog = AuthenticationDialog(context, 'alice@server');
            return const Scaffold();
          },
        ),
      ),
    );
    final trusted = dialog.show<bool>(
      (context) => AlertDialog(
        title: const Text('Trust?'),
        actions: [
          TextButton(
            onPressed: () => Navigator.pop(context, true),
            child: const Text('Trust'),
          ),
        ],
      ),
    );
    await tester.pumpAndSettle();
    dialog.close();
    expect(await trusted, isNull);
    await tester.pumpAndSettle();
    expect(find.text('Trust'), findsNothing);
    expect(dialog.active, isFalse);
  });

  testWidgets('server prompts preserve answers, echo and instructions', (
    tester,
  ) async {
    late AuthenticationDialog dialog;
    await tester.pumpWidget(
      MaterialApp(
        home: Builder(
          builder: (context) {
            dialog = AuthenticationDialog(context, 'user@server');
            return const Scaffold();
          },
        ),
      ),
    );
    final response = dialog.request(
      SSHUserInfoRequest('Sign in', 'Server instructions', [
        SSHUserInfoPrompt('Account', true),
        SSHUserInfoPrompt('Secret', false),
      ]),
      true,
    );
    await tester.pumpAndSettle();
    expect(find.text('user@server'), findsOneWidget);
    expect(find.text('Server instructions'), findsOneWidget);
    expect(find.text('Authentication failed. Try again.'), findsOneWidget);
    final fields = find.byType(TextField);
    expect(tester.widget<TextField>(fields.at(0)).obscureText, isFalse);
    expect(tester.widget<TextField>(fields.at(1)).obscureText, isTrue);
    await tester.enterText(fields.at(0), 'alice');
    await tester.enterText(fields.at(1), ' secret with spaces ');
    await tester.testTextInput.receiveAction(TextInputAction.done);
    expect(await response, ['alice', ' secret with spaces ']);
    await tester.pumpAndSettle();
  });

  testWidgets('cancel, back, and connection failure dismiss authentication', (
    tester,
  ) async {
    late AuthenticationDialog dialog;
    await tester.pumpWidget(
      MaterialApp(
        home: Builder(
          builder: (context) {
            dialog = AuthenticationDialog(context, 'user@server');
            return const Scaffold();
          },
        ),
      ),
    );
    final request = SSHUserInfoRequest('', '', [
      SSHUserInfoPrompt('Password', false),
    ]);
    final context = dialog.context;
    for (final exit in ['cancel', 'back', 'failure']) {
      dialog = AuthenticationDialog(context, 'user@server');
      final response = dialog.request(request, false);
      await tester.pumpAndSettle();
      if (exit == 'cancel') {
        await tester.tap(find.text('Cancel'));
      } else if (exit == 'back') {
        await tester.binding.handlePopRoute();
      } else {
        dialog.close();
      }
      expect(await response, isNull);
      await tester.pumpAndSettle();
      expect(find.byType(AlertDialog), findsNothing);
    }
    expect(await dialog.request(request, false), isNull);
  });

  testWidgets(
    'password profile may omit a saved password and remain editable',
    (tester) async {
      FlutterSecureStorage.setMockInitialValues({});
      Profile? saved;
      final profile = Profile(name: 'Server', host: 'server', user: 'alice');
      await tester.pumpWidget(
        MaterialApp(
          home: Builder(
            builder: (context) {
              return Scaffold(
                body: TextButton(
                  onPressed: () async {
                    saved = await Navigator.of(context).push<Profile>(
                      MaterialPageRoute(
                        builder: (_) => ProfilePage(profile: profile),
                      ),
                    );
                  },
                  child: const Text('Edit'),
                ),
              );
            },
          ),
        ),
      );
      await tester.tap(find.text('Edit'));
      await tester.pumpAndSettle();
      expect(find.text('Ask when connecting'), findsOneWidget);
      await tester.tap(find.text('Save connection'));
      await tester.pumpAndSettle();
      expect(saved?.password, isEmpty);
      expect(saved?.keyId, isEmpty);
    },
  );
}
