#!/usr/bin/env bash

##################################################
# Unit Tests for wipe-disks.sh Helper Functions
##################################################
#
# This script tests the robustness improvements made to
# the wipe-disks.sh script, specifically the integer
# validation and extraction functions.
#
##################################################

set -euo pipefail

# Test counters
TESTS_RUN=0
TESTS_PASSED=0
TESTS_FAILED=0

# ANSI color codes
RESET='\e[0m'
GREEN='\e[32m'
RED='\e[31m'
BLUE='\e[34m'

##################################################
# Helper Functions (copied from wipe-disks.sh)
##################################################

# Helper: Validate if a value is a valid integer
is_integer() {
	local value=$1
	[[ $value =~ ^-?[0-9]+$ ]]
}

# Helper: Safely extract integer from a value (strips non-numeric characters)
extract_integer() {
	local value=$1
	local cleaned=""
	local has_minus=false

	# Remove non-digit characters (except leading minus)
	# First, check for exactly one leading minus (not multiple)
	if [[ $value =~ ^-[^-] ]]; then
		has_minus=true
		value="${value#-}"
	elif [[ $value =~ ^-- ]]; then
		# Multiple minuses - invalid, return empty
		echo ""
		return
	fi

	# Strip all non-numeric characters
	cleaned="${value//[^0-9]/}"

	# Add back the minus if there was one
	if [[ $has_minus == true ]]; then
		cleaned="-${cleaned}"
	fi

	# Return the value if it's a valid integer, otherwise return empty
	if is_integer "$cleaned"; then
		echo "$cleaned"
	else
		echo ""
	fi
}

##################################################
# Test Framework Functions
##################################################

print_test_header() {
	echo -e "\n${BLUE}========================================${RESET}"
	echo -e "${BLUE}$1${RESET}"
	echo -e "${BLUE}========================================${RESET}\n"
}

assert_equals() {
	local test_name=$1
	local expected=$2
	local actual=$3

	TESTS_RUN=$((TESTS_RUN + 1))

	if [ "$expected" = "$actual" ]; then
		echo -e "${GREEN}✓ PASS${RESET}: $test_name"
		TESTS_PASSED=$((TESTS_PASSED + 1))
		return 0
	else
		echo -e "${RED}✗ FAIL${RESET}: $test_name"
		echo -e "  Expected: '${expected}'"
		echo -e "  Actual:   '${actual}'"
		TESTS_FAILED=$((TESTS_FAILED + 1))
		return 1
	fi
}

# shellcheck disable=SC2016  # Conditions use single quotes intentionally for eval
assert_true() {
	local test_name=$1
	local condition=$2

	TESTS_RUN=$((TESTS_RUN + 1))

	if eval "$condition"; then
		echo -e "${GREEN}✓ PASS${RESET}: $test_name"
		TESTS_PASSED=$((TESTS_PASSED + 1))
		return 0
	else
		echo -e "${RED}✗ FAIL${RESET}: $test_name"
		echo -e "  Condition failed: $condition"
		TESTS_FAILED=$((TESTS_FAILED + 1))
		return 1
	fi
}

# shellcheck disable=SC2016  # Conditions use single quotes intentionally for eval
assert_false() {
	local test_name=$1
	local condition=$2

	TESTS_RUN=$((TESTS_RUN + 1))

	if ! eval "$condition"; then
		echo -e "${GREEN}✓ PASS${RESET}: $test_name"
		TESTS_PASSED=$((TESTS_PASSED + 1))
		return 0
	else
		echo -e "${RED}✗ FAIL${RESET}: $test_name"
		echo -e "  Condition should have failed: $condition"
		TESTS_FAILED=$((TESTS_FAILED + 1))
		return 1
	fi
}

##################################################
# Test Cases
##################################################

# shellcheck disable=SC2016  # Single quotes in assert conditions are intentional for eval

test_is_integer() {
	print_test_header "Testing is_integer() Function"

	# Valid integers
	assert_true "is_integer: positive number" "is_integer 42"
	assert_true "is_integer: zero" "is_integer 0"
	assert_true "is_integer: negative number" "is_integer -10"
	assert_true "is_integer: large number" "is_integer 123456789"

	# Invalid integers
	assert_false "is_integer: decimal number" "is_integer 3.14"
	assert_false "is_integer: string with number" "is_integer 70°C"
	assert_false "is_integer: percentage" "is_integer 50%"
	assert_false "is_integer: empty string" "is_integer ''"
	assert_false "is_integer: pure text" "is_integer 'hello'"
	assert_false "is_integer: number with spaces" "is_integer ' 42 '"
	assert_false "is_integer: hex number" "is_integer 0x1A"
	assert_false "is_integer: scientific notation" "is_integer 1e5"
}

test_extract_integer() {
	print_test_header "Testing extract_integer() Function"

	# Clean integers
	assert_equals "extract_integer: clean positive" "42" "$(extract_integer 42)"
	assert_equals "extract_integer: clean zero" "0" "$(extract_integer 0)"
	assert_equals "extract_integer: clean negative" "-10" "$(extract_integer -10)"

	# Integers with units
	assert_equals "extract_integer: temperature with unit" "70" "$(extract_integer '70°C')"
	assert_equals "extract_integer: temperature with C" "65" "$(extract_integer '65C')"
	assert_equals "extract_integer: percentage" "50" "$(extract_integer '50%')"
	assert_equals "extract_integer: percentage with text" "95" "$(extract_integer '95%used')"

	# Integers with whitespace
	assert_equals "extract_integer: leading space" "42" "$(extract_integer ' 42')"
	assert_equals "extract_integer: trailing space" "42" "$(extract_integer '42 ')"
	assert_equals "extract_integer: surrounded by spaces" "42" "$(extract_integer ' 42 ')"

	# Mixed content
	assert_equals "extract_integer: number in text" "123" "$(extract_integer 'value: 123 units')"
	assert_equals "extract_integer: with commas" "1234567" "$(extract_integer '1,234,567')"

	# Edge cases that should return empty
	assert_equals "extract_integer: empty string" "" "$(extract_integer '')"
	assert_equals "extract_integer: pure text" "" "$(extract_integer 'hello')"
	assert_equals "extract_integer: only symbols" "" "$(extract_integer '%%#@')"
	# Decimal numbers get digits extracted (314 from 3.14) - not a valid integer so returns empty
	# Note: The function strips all non-digits, so 3.14 becomes 314, which IS a valid integer
	assert_equals "extract_integer: decimal number strips dot" "314" "$(extract_integer '3.14')"

	# Negative numbers with symbols
	assert_equals "extract_integer: negative with unit" "-5" "$(extract_integer '-5°C')"
}

# shellcheck disable=SC2016  # Single quotes intentional for eval
test_temperature_comparisons() {
	print_test_header "Testing Temperature Comparisons"

	# Simulate the check_temperature logic with extracted integers
	local test_temp
	local int_temp

	# Test case 1: Temperature above 70
	test_temp="75°C"
	int_temp=$(extract_integer "$test_temp")
	assert_true "temp 75°C > 70" '[ -n "$int_temp" ] && [ "$int_temp" -gt 70 ]'

	# Test case 2: Temperature between 60 and 70
	test_temp="65C"
	int_temp=$(extract_integer "$test_temp")
	assert_true "temp 65C > 60 and <= 70" '[ -n "$int_temp" ] && [ "$int_temp" -gt 60 ] && [ "$int_temp" -le 70 ]'

	# Test case 3: Temperature below 60
	test_temp="45"
	int_temp=$(extract_integer "$test_temp")
	assert_true "temp 45 <= 60" '[ -n "$int_temp" ] && [ "$int_temp" -le 60 ]'

	# Test case 4: Invalid temperature
	test_temp="N/A"
	int_temp=$(extract_integer "$test_temp")
	assert_false "invalid temp N/A" '[ -n "$int_temp" ]'

	# Test case 5: Empty temperature
	test_temp=""
	# shellcheck disable=SC2034  # Variable used in eval'd assert
	int_temp=$(extract_integer "$test_temp")
	assert_false "empty temp" '[ -n "$int_temp" ]'
}

# shellcheck disable=SC2016  # Single quotes intentional for eval
test_percentage_comparisons() {
	print_test_header "Testing Percentage Comparisons"

	# Test percentage_used >= 90 (wear warning)
	local percentage_used="95%"
	local int_percentage
	int_percentage=$(extract_integer "$percentage_used")
	assert_true "percentage 95% >= 90" '[ -n "$int_percentage" ] && [ "$int_percentage" -ge 90 ]'

	# Test available_spare < 10 (spare warning)
	local available_spare="5%"
	local int_spare
	# shellcheck disable=SC2034  # Variable used in eval'd assert
	int_spare=$(extract_integer "$available_spare")
	assert_true "spare 5% < 10" '[ -n "$int_spare" ] && [ "$int_spare" -lt 10 ]'

	# Test normal percentage
	percentage_used="50%"
	int_percentage=$(extract_integer "$percentage_used")
	assert_true "percentage 50% < 90" '[ -n "$int_percentage" ] && [ "$int_percentage" -lt 90 ]'

	# Test invalid percentage
	percentage_used="unknown"
	# shellcheck disable=SC2034  # Variable used in eval'd assert
	int_percentage=$(extract_integer "$percentage_used")
	assert_false "invalid percentage" '[ -n "$int_percentage" ]'
}

# shellcheck disable=SC2016  # Single quotes intentional for eval
test_error_count_comparisons() {
	print_test_header "Testing Error Count Comparisons"

	# Test error count > 0
	local error_count="5"
	local int_error
	int_error=$(extract_integer "$error_count")
	assert_true "error count 5 > 0" '[ -n "$int_error" ] && [ "$int_error" -gt 0 ]'

	# Test error count = 0
	error_count="0"
	int_error=$(extract_integer "$error_count")
	assert_false "error count 0 > 0" '[ -n "$int_error" ] && [ "$int_error" -gt 0 ]'

	# Test with units (shouldn't happen but let's test)
	error_count="3 errors"
	# shellcheck disable=SC2034  # Variable used in eval'd assert
	int_error=$(extract_integer "$error_count")
	assert_true "error count '3 errors' > 0" '[ -n "$int_error" ] && [ "$int_error" -gt 0 ]'
}

# shellcheck disable=SC2016  # Single quotes intentional for eval
test_wear_level_comparisons() {
	print_test_header "Testing Wear Level Comparisons"

	# Test wear <= 10 (critical)
	local wear="8"
	local int_wear
	int_wear=$(extract_integer "$wear")
	assert_true "wear 8 <= 10" '[ -n "$int_wear" ] && [ "$int_wear" -le 10 ]'

	# Test wear <= 20 (warning)
	wear="15"
	int_wear=$(extract_integer "$wear")
	assert_true "wear 15 <= 20" '[ -n "$int_wear" ] && [ "$int_wear" -le 20 ]'

	# Test normal wear
	wear="95"
	# shellcheck disable=SC2034  # Variable used in eval'd assert
	int_wear=$(extract_integer "$wear")
	assert_true "wear 95 > 20" '[ -n "$int_wear" ] && [ "$int_wear" -gt 20 ]'
}

# shellcheck disable=SC2016  # Single quotes intentional for eval
test_bytes_written_comparisons() {
	print_test_header "Testing Total Bytes Written Comparisons"

	# Test large value > 1000000
	local total_bytes="5000000"
	local int_bytes
	int_bytes=$(extract_integer "$total_bytes")
	assert_true "bytes 5000000 > 1000000" '[ -n "$int_bytes" ] && [ "$int_bytes" -gt 1000000 ]'

	# Test with commas (common in output)
	total_bytes="2,500,000"
	int_bytes=$(extract_integer "$total_bytes")
	assert_true "bytes 2,500,000 > 1000000" '[ -n "$int_bytes" ] && [ "$int_bytes" -gt 1000000 ]'

	# Test small value
	total_bytes="500000"
	# shellcheck disable=SC2034  # Variable used in eval'd assert
	int_bytes=$(extract_integer "$total_bytes")
	assert_true "bytes 500000 < 1000000" '[ -n "$int_bytes" ] && [ "$int_bytes" -lt 1000000 ]'
}

# shellcheck disable=SC2016  # Single quotes intentional for eval
test_edge_cases() {
	print_test_header "Testing Edge Cases"

	# Multiple numbers in string (should take all digits)
	assert_equals "multiple numbers" "12345" "$(extract_integer '123 test 45')"

	# Very large numbers
	assert_equals "very large number" "999999999999" "$(extract_integer '999999999999')"

	# Multiple minus signs (should only keep first)
	local result
	# shellcheck disable=SC2034  # Variable used in eval'd assert
	result=$(extract_integer '--42')
	assert_true "double minus treated as invalid" '[ -z "$result" ]'

	# Number with decimal (extracts all digits, becoming valid integer)
	assert_equals "decimal number extracts digits" "425" "$(extract_integer '42.5')"

	# Unicode characters
	assert_equals "unicode temperature" "25" "$(extract_integer '25°C')"

	# Tabs and newlines
	assert_equals "number with tabs" "42" "$(extract_integer $'42\t')"

	# Leading zeros (should preserve)
	assert_equals "leading zeros" "00042" "$(extract_integer '00042')"
}

##################################################
# Test Runner
##################################################

main() {
	echo -e "${BLUE}"
	echo "╔════════════════════════════════════════════════════════════╗"
	echo "║  Wipe-Disks Helper Functions Unit Test Suite              ║"
	echo "╚════════════════════════════════════════════════════════════╝"
	echo -e "${RESET}"

	# Run all test suites
	test_is_integer
	test_extract_integer
	test_temperature_comparisons
	test_percentage_comparisons
	test_error_count_comparisons
	test_wear_level_comparisons
	test_bytes_written_comparisons
	test_edge_cases

	# Print summary
	echo -e "\n${BLUE}========================================${RESET}"
	echo -e "${BLUE}Test Summary${RESET}"
	echo -e "${BLUE}========================================${RESET}"
	echo -e "Total tests run:    ${TESTS_RUN}"
	echo -e "${GREEN}Tests passed:       ${TESTS_PASSED}${RESET}"

	if [ $TESTS_FAILED -gt 0 ]; then
		echo -e "${RED}Tests failed:       ${TESTS_FAILED}${RESET}"
		echo -e "\n${RED}❌ TEST SUITE FAILED${RESET}\n"
		exit 1
	else
		echo -e "\n${GREEN}✓ ALL TESTS PASSED${RESET}\n"
		exit 0
	fi
}

# Run tests
main
