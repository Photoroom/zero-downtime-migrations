# Trivial passing migration - should not trigger any rule.
from django.db import migrations, models


class Migration(migrations.Migration):

    initial = True

    dependencies = []

    operations = [
        migrations.CreateModel(
            name='Order',
            fields=[
                ('id', models.BigAutoField(primary_key=True)),
            ],
        ),
    ]
