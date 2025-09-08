#!/usr/bin/env bash

# This script automates the entire LDAP testing process for Conduit.
# It starts a temporary LDAP server, creates a temporary Conduit config,
# starts the Conduit server, runs API tests, and cleans everything up.

set -e

# --- (REQUIRED) USER CONFIGURATION ---
# !!! IMPORTANT !!!
# You MUST set this variable to the absolute path of your Conduit project directory.
CONDUIT_PROJECT_DIR="/home/deji/Documents/Code/conduit"
# --- END OF USER CONFIGURATION ---


# --- Script Configuration ---
CONDUIT_SERVER_URL="http://localhost:6167"
CONDUIT_SERVER_NAME="localhost" # Must match the server_name in the temp config

# Get the directory where this script is located to resolve other file paths
SCRIPT_DIR=$( cd -- "$( dirname -- "${BASH_SOURCE[0]}" )" &> /dev/null && pwd )
LDAP_COMPOSE_FILE="$CONDUIT_PROJECT_DIR/docker-compose.ldap.yml"
LDAP_USER_FILE="$CONDUIT_PROJECT_DIR/user.ldif"
TEST_CONFIG_FILE="$CONDUIT_PROJECT_DIR/conduit.test.toml"

CONDUIT_PID=""

# --- Colors ---
GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# --- Helper Functions ---

function cleanup() {
    printf "\n${YELLOW}--> Cleaning up test environment...${NC}\n"
    if [ -n "$CONDUIT_PID" ]; then
        printf "Stopping Conduit server (PID: $CONDUIT_PID)...
"
        kill "$CONDUIT_PID" || true
    fi
    
    printf "Stopping LDAP server...
"
    docker-compose -f "$LDAP_COMPOSE_FILE" down -v --remove-orphans
    
    printf "Removing temporary config file...
"
    rm -f "$TEST_CONFIG_FILE"
    
    printf "${GREEN}Cleanup complete.${NC}\n"
}

# Set a trap to run the cleanup function on script exit (even on failure)
trap cleanup EXIT

function check_deps() {
    for cmd in docker-compose ldapadd cargo jq; do
        if ! command -v "$cmd" &> /dev/null; then
            printf "${RED}Error: Required command '$cmd' is not installed. Please install it to continue.${NC}\n"
            exit 1
        fi
    done
}

function test_login() {
    local username="$1"
    local password="$2"
    local description="$3"
    local expected_http_status="$4"

    printf "\n${YELLOW}--> TEST: $description${NC}\n"

    HTTP_STATUS=$(curl -s -o /dev/null -w "%{http_code}" -XPOST \
        "$CONDUIT_SERVER_URL/_matrix/client/v3/login" \
        -H "Content-Type: application/json" \
        --data @- << EOF
{
    "type": "m.login.password",
    "identifier": { "type": "m.id.user", "user": "$username" },
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
}

# --- Main Script ---

check_deps

printf "${YELLOW}--- Step 1: Setting up Test LDAP Server ---${NC}\n"
docker-compose -f "$LDAP_COMPOSE_FILE" up -d
printf "Waiting for LDAP server to initialize...
"
sleep 5 # Give the LDAP container time to start
ldapadd -x -D "cn=admin,dc=conduit,dc=rs" -w admin -H ldap://localhost -f "$LDAP_USER_FILE"
printf "${GREEN}LDAP server is running and test user is created.${NC}\n"

printf "\n${YELLOW}--- Step 2: Creating Temporary Conduit Config ---${NC}\n"
cat <<EOF > "$TEST_CONFIG_FILE"
# This is a temporary config file for testing purposes.
# It will be deleted automatically after the test.

[global]
server_name = "$CONDUIT_SERVER_NAME"
database_path = "/tmp/conduit-test-db"
port = 6167
allow_registration = false
allow_federation = false

[ldap]
enabled = true
uri = "ldap://localhost:389"
bind_dn = "cn=admin,dc=conduit,dc=rs"
bind_password = "admin"
base_dn = "ou=users,dc=conduit,dc=rs"
user_filter = "(uid=%u)"
attribute_mapping = { localpart = "uid", displayname = "cn", email = "mail" }
EOF
printf "${GREEN}Temporary config written to $TEST_CONFIG_FILE${NC}\n"

printf "\n${YELLOW}--- Step 3: Starting Conduit Server ---${NC}\n"
# Start conduit in the background
CONDUIT_CONFIG="$TEST_CONFIG_FILE" cargo run --manifest-path "$CONDUIT_PROJECT_DIR/Cargo.toml" --release &
CONDUIT_PID=$!
printf "Conduit server starting in background (PID: $CONDUIT_PID).
"
printf "Waiting for Conduit to initialize...
"
sleep 10 # Give Conduit time to compile and start

printf "\n${YELLOW}--- Step 4: Running Authentication Tests ---${NC}\n"
test_login "testuser" "password" "First-time login for valid LDAP user" 200
test_login "testuser" "password" "Subsequent login for valid LDAP user" 200
test_login "testuser" "wrongpassword" "Login with incorrect password for LDAP user" 401
test_login "nosuchuser" "password" "Login for a user that does not exist" 401

printf "\n${GREEN}--- All automated tests passed successfully! ---${NC}\n"

