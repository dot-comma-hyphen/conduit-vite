#!/usr/bin/env bash

set -e

# --- Configuration ---
CONDUIT_SERVER_URL="http://localhost:6167"
# This must match the server_name in your conduit.toml
CONDUIT_SERVER_NAME="localhost" 

# --- Colors ---
GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# --- Helper Functions ---
function check_jq() {
    if ! command -v jq &> /dev/null; then
        printf "${RED}Error: 'jq' is not installed. Please install it to run this script.${NC}\n"
        printf "e.g., 'sudo apt-get install jq' or 'sudo yum install jq'\n"
        exit 1
    fi
}

function test_login() {
    local username="$1"
    local password="$2"
    local description="$3"
    local expected_http_status="$4"
    local expected_matrix_error="$5"

    printf "\n${YELLOW}--> TEST: $description${NC}\n"

    HTTP_STATUS=$(curl -s -o /dev/null -w "%{http_code}" -XPOST \
        "$CONDUIT_SERVER_URL/_matrix/client/v3/login" \
        -H "Content-Type: application/json" \
        --data @- << EOF
{
    "type": "m.login.password",
    "identifier": {
        "type": "m.id.user",
        "user": "$username"
    },
    "password": "$password",
    "device_id": "LDAPTEST"
}
EOF
    )

    if [ "$HTTP_STATUS" -eq "$expected_http_status" ]; then
        printf "${GREEN}SUCCESS: Received expected HTTP status $HTTP_STATUS${NC}\n"
    else
        printf "${RED}FAILURE: Expected HTTP status $expected_http_status, but got $HTTP_STATUS${NC}\n"
        exit 1
    fi

    # For failed logins, also check the error code
    if [ "$expected_http_status" -ne 200 ]; then
        RESPONSE_BODY=$(curl -s -XPOST \
            "$CONDUIT_SERVER_URL/_matrix/client/v3/login" \
            -H "Content-Type: application/json" \
            --data @- << EOF
{
    "type": "m.login.password",
    "identifier": {
        "type": "m.id.user",
        "user": "$username"
    },
    "password": "$password"
}
EOF
        )
        
        MATRIX_ERROR=$(echo "$RESPONSE_BODY" | jq -r '.errcode')
        if [ "$MATRIX_ERROR" == "$expected_matrix_error" ]; then
            printf "${GREEN}SUCCESS: Received expected Matrix error '$MATRIX_ERROR'${NC}\n"
        else
            printf "${RED}FAILURE: Expected Matrix error '$expected_matrix_error', but got '$MATRIX_ERROR'${NC}\n"
            echo "Full response: $RESPONSE_BODY"
            exit 1
        fi
    fi
}

# --- Main Script ---
check_jq

printf "${YELLOW}Starting LDAP authentication tests...${NC}\n"

# Test Case 1: First-time successful login for LDAP user
test_login "testuser" "password" "First-time login for valid LDAP user" 200

# Test Case 2: Subsequent successful login for LDAP user
test_login "testuser" "password" "Subsequent login for valid LDAP user" 200

# Test Case 3: Incorrect password for LDAP user
test_login "testuser" "wrongpassword" "Login with incorrect password for LDAP user" 401 "M_FORBIDDEN"

# Test Case 4: Non-existent user
test_login "nosuchuser" "password" "Login for a user that does not exist" 401 "M_FORBIDDEN"

printf "\n${GREEN}All automated tests passed successfully!${NC}\n"
printf "\n${YELLOW}Manual Test Required:${NC} Don't forget to manually test the fallback for a non-LDAP local user.\n"
