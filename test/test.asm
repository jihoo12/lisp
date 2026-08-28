mov rax, 0
loop:
add rax, 1
cmp rax, 5
jne loop
ret