import 'dart:io';

String releaseVersionOutput(Iterable<String> tags, DateTime now) {
  final pattern = RegExp(r'android-.+\+([0-9]+)$');
  var highest = BigInt.one;
  for (final tag in tags) {
    final match = pattern.matchAsPrefix(tag);
    if (match == null || match.end != tag.length) continue;
    final code = BigInt.parse(match[1]!);
    if (code > highest) highest = code;
  }
  final code = highest + BigInt.one;
  if (code > BigInt.from(999999999)) {
    throw StateError('Android version code exhausted');
  }
  final utc = now.toUtc();
  final name = [
    utc.year % 100,
    utc.month,
    utc.day,
    utc.hour,
    utc.minute,
  ].map((part) => part.toString().padLeft(2, '0')).join('.');
  return 'name=$name\ncode=$code\ntag=android-$name+$code\n';
}

void main(List<String> arguments) {
  if (arguments.length != 2) {
    throw ArgumentError('Usage: release_version.dart TAGS_FILE OUTPUT_FILE');
  }
  final output = releaseVersionOutput(
    File(arguments[0]).readAsLinesSync(),
    DateTime.now().toUtc(),
  );
  File(arguments[1]).writeAsStringSync(output, mode: FileMode.append);
}
