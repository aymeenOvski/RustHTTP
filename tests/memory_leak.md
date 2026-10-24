# Kill any existing server instances to free up ports

    pkill -f localserver

# Launch your server inside Valgrind

    valgrind --leak-check=full --show-leak-kinds=all --track-origins=yes ./target/debug/localserver

# Run your stress-testing script in a separate terminal to hammer the server with concurrent requests

    python3 tests/stress.py

# After shutting down, Valgrind will output a LEAK SUMMARY

### output example :

    ==768692== LEAK SUMMARY:
    ==768692==    definitely lost: 0 bytes in 0 blocks
    ==768692==    indirectly lost: 0 bytes in 0 blocks
    ==768692==      possibly lost: 1,117,966 bytes in 5,022 blocks
    ==768692==    still reachable: 17,063 bytes in 52 blocks
    ==768692==         suppressed: 0 bytes in 0 blocks
