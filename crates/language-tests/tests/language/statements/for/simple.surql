/**
[test]

[[test.results]]
value = "NONE"

[[test.results]]
value = "[{ id: person:1, test: 1 }]"

[[test.results]]
value = "NONE"

[[test.results]]
value = "[{ id: person:1, test: 1 }, { id: person:4, test: 4 }, { id: person:6, test: 6 }]"

[[test.results]]
error = "An error occurred: This is an error"

[[test.results]]
value = "[{ id: person:1, test: 1 }, { id: person:4, test: 4 }, { id: person:6, test: 6 }]"

*/

OPTION FUTURES = true;

FOR $test IN [1, 2, 3] {
	IF $test == 2 {
		BREAK;
	};
	UPSERT type::thing('person', $test) SET test = $test;
};
SELECT * FROM person;

FOR $test IN [4, 5, 6] {
	IF $test == 5 {
		CONTINUE;
	};
	UPSERT type::thing('person', $test) SET test = $test;
};
SELECT * FROM person;

FOR $test IN <future> { [7, 8, 9] } {
	IF $test > 8 {
		THROW 'This is an error';
	};
	UPSERT type::thing('person', $test) SET test = $test;
};
SELECT * FROM person;
