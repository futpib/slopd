import 'package:flutter/material.dart';
import 'connections.dart';
import 'ui.dart';
export 'connections.dart';
export 'dashboard.dart';

void main() => runApp(const SlopApp());

class SlopApp extends StatelessWidget {
  const SlopApp({super.key});
  @override
  Widget build(BuildContext context) => MaterialApp(
    title: 'slop',
    debugShowCheckedModeBanner: false,
    theme: appTheme(Brightness.light),
    darkTheme: appTheme(Brightness.dark),
    themeMode: ThemeMode.dark,
    home: const ConnectionsPage(),
  );
}
