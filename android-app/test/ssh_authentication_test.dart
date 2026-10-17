import 'dart:convert';
import 'dart:io';

import 'package:dartssh2/dartssh2.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:slopd_control/control.dart';

void main() {
  test('uses server prompts and falls back for password-only SSH', () async {
    final python = Platform.environment['SSH_AUTH_PYTHON'] ?? 'python3';
    final server = await Process.start(python, [
      'test/fixtures/ssh_auth_server.py',
    ]);
    addTearDown(() async {
      server.kill();
      await server.exitCode;
    });
    final portLine = await server.stdout
        .transform(utf8.decoder)
        .transform(const LineSplitter())
        .first
        .timeout(const Duration(seconds: 10));
    final port = (jsonDecode(portLine) as Map<String, dynamic>)['port'] as int;

    var passwordRequests = 0;
    final promptNames = <String>[];
    final interactive = await SshControl.connect(
      Profile(name: 'test', host: '127.0.0.1', port: port, user: 'interactive'),
      (_, _) async => true,
      onAuthenticationRequest: (request, retry) async {
        expect(retry, isFalse);
        promptNames.add(request.name);
        return switch (request.name) {
          'Server sign-in' => ['test account', 'test password'],
          'Verification' => ['123456'],
          'SSH password' => <String>['unexpected'],
          _ => null,
        };
      },
    ).timeout(const Duration(seconds: 10));
    expect(promptNames, ['Server sign-in', 'Verification']);
    await interactive.close();

    final password = await SshControl.connect(
      Profile(name: 'test', host: '127.0.0.1', port: port, user: 'password'),
      (_, _) async => true,
      onAuthenticationRequest: (request, retry) async {
        expect(retry, isFalse);
        expect(request, isA<SSHUserInfoRequest>());
        expect(request.name, 'SSH password');
        passwordRequests++;
        return ['test password'];
      },
    ).timeout(const Duration(seconds: 10));
    expect(passwordRequests, 1);
    await password.close();
  });
}
