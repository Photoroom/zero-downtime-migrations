# R011: RenameField - should warn
from django.db import migrations


class Migration(migrations.Migration):

    dependencies = [
        ('myapp', '0008'),
    ]

    operations = [
        migrations.RenameField(
            model_name='order',
            old_name='old_field_name',
            new_name='new_field_name',
        ),
    ]
