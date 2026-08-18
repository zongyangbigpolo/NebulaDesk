import 'package:flutter_test/flutter_test.dart';

import 'package:nebula_manager/main.dart';

void main() {
  testWidgets('shows the Nebula connection manager', (WidgetTester tester) async {
    await tester.pumpWidget(const NebulaManagerApp());
    await tester.pumpAndSettle();

    expect(find.text('Nebula — Connections'), findsOneWidget);
    expect(find.text('No VDAs yet. Tap + to add one.'), findsOneWidget);
  });
}
