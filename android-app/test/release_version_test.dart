import 'dart:io';

import 'package:flutter_test/flutter_test.dart';

import '../scripts/release_version.dart' as release;

void main() {
  test('starts at code 2 and formats the UTC release date', () {
    expect(
      release.releaseVersionOutput(
        [],
        DateTime.parse('2026-01-02T01:04:00+02:00'),
      ),
      'name=26.01.01.23.04\ncode=2\ntag=android-26.01.01.23.04+2\n',
    );
  });

  test('increments the highest complete Android release tag', () {
    expect(
      release.releaseVersionOutput([
        'android-old+007',
        'android-old+2',
        'android-old+0',
        'v1.0+900',
        'android-+900',
        'android-old+999suffix',
        'prefix-android-old+900',
        'android-old+-900',
        'android-old+900\n',
      ], DateTime.utc(2026, 11, 15, 9, 5)),
      'name=26.11.15.09.05\ncode=8\ntag=android-26.11.15.09.05+8\n',
    );
  });

  test(
    'allows the last version code and rejects exhaustion without overflow',
    () {
      expect(
        release.releaseVersionOutput([
          'android-old+999999998',
        ], DateTime.utc(2026)),
        contains('code=999999999\n'),
      );
      for (final code in ['999999999', '999999999999999999999999999999']) {
        expect(
          () => release.releaseVersionOutput([
            'android-old+$code',
          ], DateTime.utc(2026)),
          throwsStateError,
        );
      }
    },
  );

  test('appends workflow outputs and leaves them intact on exhaustion', () {
    final root = Directory.systemTemp.createTempSync('slopd-release-test-');
    addTearDown(() => root.deleteSync(recursive: true));
    final tags = File('${root.path}/tags')
      ..writeAsStringSync('android-old+12\n');
    final output = File('${root.path}/output')
      ..writeAsStringSync('previous=value\n');
    release.main([tags.path, output.path]);
    final saved = output.readAsStringSync();
    expect(saved, startsWith('previous=value\nname='));
    expect(saved, contains('\ncode=13\ntag=android-'));
    tags.writeAsStringSync('android-old+999999999\n');
    expect(() => release.main([tags.path, output.path]), throwsStateError);
    expect(output.readAsStringSync(), saved);
  });
}
