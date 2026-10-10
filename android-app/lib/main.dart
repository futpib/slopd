import 'package:flutter/material.dart';
import 'connections.dart';
import 'ui.dart';
export 'connections.dart';
export 'dashboard.dart';

void main() => runApp(const SlopdApp());

class SlopdApp extends StatelessWidget {
  const SlopdApp({super.key});
  @override
  Widget build(BuildContext context) => MaterialApp(
    title: 'slopd',
    debugShowCheckedModeBanner: false,
    theme: appTheme(Brightness.light),
    darkTheme: appTheme(Brightness.dark),
    themeMode: ThemeMode.dark,
    home: const ConnectionsPage(),
  );
}
